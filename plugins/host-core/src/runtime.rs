//! A bounded, one-event JSON-RPC subprocess exchange.

use std::future::Future;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const MAX_OUTPUT: u64 = 64 * 1024;
const MAX_TEXT: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageReceived {
    pub account_id: String,
    pub chat_id: String,
    pub message_id: String,
}

/// Run one event in a fresh process, without blocking a chat or the UI.
///
/// The application callback must check that the account remains enabled and
/// that the current draft, including unsaved editor and attachment state, is
/// empty before using the proposed text. Its boolean becomes the RPC result.
pub async fn request_draft<F, Fut>(
    executable: &Path,
    event: &MessageReceived,
    deadline: Duration,
    apply: F,
) -> Result<Option<bool>>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = bool>,
{
    ensure!(
        !event.account_id.is_empty() && !event.chat_id.is_empty() && !event.message_id.is_empty(),
        "empty event identifier"
    );
    let mut command = tokio::process::Command::new(executable);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut attempts = 0;
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            Err(error)
                if cfg!(target_os = "linux")
                    && error.raw_os_error() == Some(26)
                    && attempts < 3 =>
            {
                // An executable just published on some filesystems can
                // briefly remain busy after its writer closes.
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error).context("start plugin"),
        }
    };
    let mut stdin = child.stdin.take().context("missing plugin stdin")?;
    let stdout = child.stdout.take().context("missing plugin stdout")?;
    let notification = json!({"jsonrpc":"2.0","method":"message.received","params":event});
    tokio::time::timeout(deadline, async {
        stdin.write_all(notification.to_string().as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        let mut output = Vec::new();
        BufReader::new(stdout)
            .take(MAX_OUTPUT + 1)
            .read_until(b'\n', &mut output)
            .await?;
        ensure!(output.len() as u64 <= MAX_OUTPUT, "plugin output too large");
        if output.is_empty() {
            return Ok(None);
        }
        let request = serde_json::from_slice::<Value>(&output);
        if request.as_ref().is_ok_and(|request| {
            request["jsonrpc"] == "2.0"
                && request["method"].is_string()
                && request.get("id").is_none()
        }) {
            // A JSON-RPC notification has no response and cannot create a
            // draft because the plugin would not receive the outcome.
            return Ok(None);
        }
        let id = request.as_ref().ok().map_or(Value::Null, |request| {
            if request["id"].is_string() || request["id"].is_number() {
                request["id"].clone()
            } else {
                Value::Null
            }
        });
        let error = match &request {
            Err(_) => Some((-32700, "Parse error")),
            Ok(request) if request["jsonrpc"] != "2.0" || id.is_null() => {
                Some((-32600, "Invalid Request"))
            }
            Ok(request) if request["method"] != "draft.set_if_empty" => {
                Some((-32601, "Method not found"))
            }
            Ok(request) => {
                let params = &request["params"];
                let text = params["text"].as_str();
                if params["account_id"] != event.account_id
                    || params["chat_id"] != event.chat_id
                    || params["message_id"] != event.message_id
                    || !text.is_some_and(|text| !text.is_empty() && text.len() <= MAX_TEXT)
                {
                    Some((-32602, "Invalid params"))
                } else {
                    None
                }
            }
        };
        let (response, outcome) = if let Some((code, message)) = error {
            (
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
                None,
            )
        } else {
            let request = request.expect("validated request");
            let text = request["params"]["text"].as_str().expect("validated text");
            let applied = apply(text.to_owned()).await;
            (
                json!({"jsonrpc":"2.0","id":id,"result":{"applied":applied}}),
                Some(applied),
            )
        };
        stdin.write_all(response.to_string().as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.shutdown().await?;
        drop(stdin);
        let status = child.wait().await?;
        ensure!(status.success(), "plugin failed");
        if outcome.is_none() {
            bail!("invalid plugin request");
        }
        Ok(outcome)
    })
    .await
    .context("plugin timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[tokio::test]
    async fn example_returns_a_draft_request() {
        let directory = tempfile::tempdir().unwrap();
        let package = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../example-reply/example-linux-x86_64.zapplugin");
        let manifest = crate::install(&package, directory.path(), "linux-x86_64").unwrap();
        let exe = crate::executable(directory.path(), &manifest, "linux-x86_64").unwrap();
        let event = MessageReceived {
            account_id: "account-a".into(),
            chat_id: "chat-b".into(),
            message_id: "message-c".into(),
        };
        let text = request_draft(&exe, &event, Duration::from_secs(5), |text| async move {
            text == "en breve te responderemos"
        })
        .await
        .unwrap();
        assert_eq!(text, Some(true));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hung_process_is_timed_out() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let exe = directory.path().join("hang");
        std::fs::write(&exe, "#!/bin/sh\nwhile :; do :; done\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let event = MessageReceived {
            account_id: "a".into(),
            chat_id: "b".into(),
            message_id: "c".into(),
        };
        assert!(
            request_draft(&exe, &event, Duration::from_millis(50), |_| async { true })
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_other_chats_and_send_methods() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let exe = directory.path().join("bad-plugin");
        let event = MessageReceived {
            account_id: "a".into(),
            chat_id: "b".into(),
            message_id: "c".into(),
        };
        for request in [
            r#"{"jsonrpc":"2.0","id":1,"method":"draft.set_if_empty","params":{"account_id":"a","chat_id":"other","message_id":"c","text":"x"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"message.send","params":{"account_id":"a","chat_id":"b","message_id":"c","text":"x"}}"#,
        ] {
            std::fs::write(
                &exe,
                format!("#!/bin/sh\nread -r event\nprintf '%s\\n' '{request}'\n"),
            )
            .unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
            let called = std::sync::atomic::AtomicBool::new(false);
            assert!(
                request_draft(&exe, &event, Duration::from_secs(1), |_| async {
                    called.store(true, std::sync::atomic::Ordering::SeqCst);
                    true
                })
                .await
                .is_err()
            );
            assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn responds_with_application_outcome() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let exe = directory.path().join("plugin");
        std::fs::write(
            &exe,
            r#"#!/bin/sh
read -r event
printf '%s\n' '{"jsonrpc":"2.0","id":17,"method":"draft.set_if_empty","params":{"account_id":"a","chat_id":"b","message_id":"c","text":"x"}}'
read -r response
case "$response" in
  *'"id":17'*'"applied":false'*) exit 0 ;;
  *'"applied":false'*'"id":17'*) exit 0 ;;
  *) exit 1 ;;
esac
"#,
        ).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let event = MessageReceived {
            account_id: "a".into(),
            chat_id: "b".into(),
            message_id: "c".into(),
        };
        assert_eq!(
            request_draft(&exe, &event, Duration::from_secs(1), |_| async { false })
                .await
                .unwrap(),
            Some(false)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unsupported_method_receives_json_rpc_error() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let exe = directory.path().join("plugin");
        std::fs::write(
            &exe,
            r#"#!/bin/sh
read -r event
printf '%s\n' '{"jsonrpc":"2.0","id":17,"method":"message.send","params":{}}'
read -r response
case "$response" in
  *'"code":-32601'*) exit 0 ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let event = MessageReceived {
            account_id: "a".into(),
            chat_id: "b".into(),
            message_id: "c".into(),
        };
        let error = request_draft(&exe, &event, Duration::from_secs(1), |_| async { true })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid plugin request"));
    }
}

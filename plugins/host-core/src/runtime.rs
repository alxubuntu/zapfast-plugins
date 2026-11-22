//! A bounded, one-event subprocess exchange. Not yet wired into ZapFast.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
/// Returning text is only a proposal. The application must still check that
/// the account remains enabled and the current draft, including unsaved
/// editor and attachment state, is empty before using it.
pub async fn request_draft(
    executable: &Path,
    event: &MessageReceived,
    deadline: Duration,
) -> Result<Option<String>> {
    ensure!(
        !event.account_id.is_empty() && !event.chat_id.is_empty() && !event.message_id.is_empty(),
        "empty event identifier"
    );
    let mut child = tokio::process::Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start plugin")?;
    let mut stdin = child.stdin.take().context("missing plugin stdin")?;
    let stdout = child.stdout.take().context("missing plugin stdout")?;
    let notification = json!({"jsonrpc":"2.0","method":"message.received","params":event});
    let result = tokio::time::timeout(deadline, async {
        stdin.write_all(notification.to_string().as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.shutdown().await?;
        drop(stdin);
        let mut output = Vec::new();
        stdout.take(MAX_OUTPUT + 1).read_to_end(&mut output).await?;
        ensure!(output.len() as u64 <= MAX_OUTPUT, "plugin output too large");
        let status = child.wait().await?;
        ensure!(status.success(), "plugin failed");
        if output.is_empty() {
            return Ok(None);
        }
        let request: Value = serde_json::from_slice(&output).context("invalid JSON-RPC")?;
        ensure!(
            request["jsonrpc"] == "2.0"
                && request["method"] == "draft.set_if_empty"
                && (request["id"].is_string() || request["id"].is_number()),
            "unsupported JSON-RPC request"
        );
        let params = &request["params"];
        ensure!(
            params["account_id"] == event.account_id
                && params["chat_id"] == event.chat_id
                && params["message_id"] == event.message_id,
            "request does not match event"
        );
        let text = params["text"].as_str().context("missing draft text")?;
        ensure!(
            !text.is_empty() && text.len() <= MAX_TEXT,
            "invalid draft text"
        );
        Ok(Some(text.to_owned()))
    })
    .await
    .context("plugin timed out")?;
    result
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
        let text = request_draft(&exe, &event, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(text.as_deref(), Some("en breve te responderemos"));
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
            request_draft(&exe, &event, Duration::from_millis(50))
                .await
                .is_err()
        );
    }
}

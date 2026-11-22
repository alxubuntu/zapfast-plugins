use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut request_id = 0u64;
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if event["jsonrpc"] != "2.0" || event["method"] != "message.received" {
            continue;
        }
        let params = &event["params"];
        if !["account_id", "chat_id", "message_id"]
            .iter()
            .all(|key| params[*key].as_str().is_some_and(|value| !value.is_empty()))
        {
            continue;
        }
        let incoming = params["text"].as_str().unwrap_or_default();
        if incoming.trim().is_empty() {
            continue;
        }
        let history = params["history"]
            .as_array()
            .map_or(0, Vec::len);
        let excerpt: String = incoming.chars().take(800).collect();
        let history_label = if history == 1 {
            "mensaje anterior"
        } else {
            "mensajes anteriores"
        };
        request_id = request_id.wrapping_add(1);
        let request = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "draft.set_if_empty",
            "params": {
                "account_id": params["account_id"],
                "chat_id": params["chat_id"],
                "message_id": params["message_id"],
                "text": format!(
                    "Recibí tu mensaje: “{excerpt}”. Hay {history} {history_label} en esta conversación."
                )
            }
        });
        writeln!(stdout, "{request}")?;
        stdout.flush()?;
    }
    Ok(())
}

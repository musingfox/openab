// fake_acp_agent — minimal ACP agent fixture for e2e tests.
//
// Line-delimited JSON over stdin/stdout (ACP protocol subset):
//   initialize    -> respond with agentCapabilities
//   session/new   -> respond with sessionId
//   session/prompt-> send one agent_message_chunk notification + final response
//
// All other methods receive a generic empty-result response.

use std::io::{self, BufRead, Write};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = msg
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();

        match method.as_str() {
            "initialize" => {
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "agentInfo": {"name": "fake-agent", "version": "0.0.1"},
                        "agentCapabilities": {}
                    }
                });
                writeln!(out, "{}", resp).unwrap();
                out.flush().unwrap();
            }
            "session/new" => {
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "sessionId": "s1"
                    }
                });
                writeln!(out, "{}", resp).unwrap();
                out.flush().unwrap();
            }
            "session/prompt" => {
                // First: send notification (no id) with agent_message_chunk
                // The table markdown: "| col |\n| --- |\n| #**a>b** |"
                let notification = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "s1",
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {
                                "type": "text",
                                "text": "| col |\n| --- |\n| #**a>b** |"
                            }
                        }
                    }
                });
                writeln!(out, "{}", notification).unwrap();
                out.flush().unwrap();

                // Then: send final id-bearing response
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "stopReason": "end_turn"
                    }
                });
                writeln!(out, "{}", resp).unwrap();
                out.flush().unwrap();
            }
            "session/cancel" => {
                // notification (no id), no reply needed
            }
            _ => {
                // Generic response for any other method
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {}
                });
                writeln!(out, "{}", resp).unwrap();
                out.flush().unwrap();
            }
        }
    }
}

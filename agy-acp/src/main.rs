use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use tokio::process::Command;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    id: Option<u64>,
    method: Option<String>,
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcNotification {
    jsonrpc: &'static str,
    method: String,
    params: Value,
}

struct Session {
    has_history: bool,
}

struct Adapter {
    sessions: HashMap<String, Session>,
    working_dir: String,
}

impl Adapter {
    fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            working_dir: std::env::var("AGY_WORKING_DIR")
                .unwrap_or_else(|_| "/tmp".to_string()),
        }
    }

    fn handle_initialize(&self, id: u64) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({
                "protocolVersion": 1,
                "agentInfo": { "name": "agy", "version": env!("CARGO_PKG_VERSION") },
                "agentCapabilities": { "streaming": true, "loadSession": false },
            })),
            error: None,
        }
    }

    fn handle_session_new(&mut self, id: u64) -> JsonRpcResponse {
        let session_id = Uuid::new_v4().to_string();
        self.sessions.insert(session_id.clone(), Session { has_history: false });
        JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({ "sessionId": session_id })),
            error: None,
        }
    }

    async fn handle_session_prompt(&mut self, id: u64, params: &Value) -> Vec<String> {
        let session_id = params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
        let prompt_text = params
            .get("prompt")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .filter(|t| !t.starts_with("<sender_context>"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let clean_prompt = prompt_text.trim();

        let mut args: Vec<String> = Vec::new();
        if let Some(session) = self.sessions.get(session_id) {
            if session.has_history {
                args.push("--continue".to_string());
            }
        }
        args.push("-p".to_string());
        args.push(clean_prompt.to_string());

        let result = Command::new("agy")
            .args(&args)
            .current_dir(&self.working_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await;

        let mut output_lines = Vec::new();

        match result {
            Ok(output) => {
                let text = String::from_utf8_lossy(&output.stdout).to_string();
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.has_history = true;
                }
                let notification = serde_json::to_string(&JsonRpcNotification {
                    jsonrpc: "2.0",
                    method: "session/update".to_string(),
                    params: json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": text },
                        },
                    }),
                }).unwrap();
                output_lines.push(notification);
                let resp = JsonRpcResponse { jsonrpc: "2.0", id, result: Some(json!({ "stopReason": "end_turn" })), error: None };
                output_lines.push(serde_json::to_string(&resp).unwrap());
            }
            Err(e) => {
                let resp = JsonRpcResponse { jsonrpc: "2.0", id, result: None, error: Some(json!({"code":-32000,"message":format!("failed to run agy: {e}")})) };
                output_lines.push(serde_json::to_string(&resp).unwrap());
            }
        }
        output_lines
    }
}

#[tokio::main]
async fn main() {
    let mut adapter = Adapter::new();

    // Read stdin lines in a blocking thread, send to async handler
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) if !l.trim().is_empty() => {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
                _ => {}
            }
        }
    });

    let mut stdout = io::stdout();

    while let Some(line) = rx.recv().await {
        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let id = match req.id {
            Some(id) => id,
            None => continue,
        };

        let output = match req.method.as_deref() {
            Some("initialize") => {
                vec![serde_json::to_string(&adapter.handle_initialize(id)).unwrap()]
            }
            Some("session/new") => {
                vec![serde_json::to_string(&adapter.handle_session_new(id)).unwrap()]
            }
            Some("session/prompt") => {
                let params = req.params.unwrap_or(json!({}));
                adapter.handle_session_prompt(id, &params).await
            }
            Some("session/cancel") => {
                let r = JsonRpcResponse { jsonrpc: "2.0", id, result: Some(json!({})), error: None };
                vec![serde_json::to_string(&r).unwrap()]
            }
            Some(method) => {
                let r = JsonRpcResponse { jsonrpc: "2.0", id, result: None, error: Some(json!({"code":-32601,"message":format!("method not found: {method}")})) };
                vec![serde_json::to_string(&r).unwrap()]
            }
            None => continue,
        };

        for line in output {
            let _ = writeln!(stdout, "{}", line);
        }
        let _ = stdout.flush();
    }
}

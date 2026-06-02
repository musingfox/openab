use crate::agent::Agent;
use crate::llm::AnthropicProvider;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub id: Option<u64>,
    pub method: Option<String>,
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: Value,
}

pub struct AcpServer {
    // TODO(v0.2): add session TTL and periodic cleanup to prevent OOM
    sessions: HashMap<String, Agent>,
    working_dir: String,
    /// Active model name (safe alternative to env mutation)
    active_model: Option<String>,
    /// Active provider name: "anthropic" or "openai" (safe alternative to env mutation)
    active_provider: Option<String>,
}

impl AcpServer {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            working_dir: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "/tmp".to_string()),
            active_model: None,
            active_provider: None,
        }
    }

    pub async fn run(&mut self) {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        std::thread::spawn(move || {
            let stdin = io::stdin();
            for line in stdin.lock().lines() {
                #[allow(clippy::collapsible_match)]
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
                Some("initialize") => vec![self.handle_initialize(id)],
                Some("session/new") => vec![self.handle_session_new(id).await],
                Some("session/prompt") => {
                    let params = req.params.unwrap_or(json!({}));
                    self.handle_session_prompt(id, &params).await
                }
                Some("session/cancel") => {
                    // TODO(v0.2): implement cancellation token to abort in-progress agent.run()
                    vec![self.ok_response(id, json!({}))]
                }
                Some("session/set_config_option") => {
                    let params = req.params.unwrap_or(json!({}));
                    vec![self.handle_set_config_option(id, &params)]
                }
                Some(method) => {
                    vec![self.error_response(id, -32601, &format!("method not found: {method}"))]
                }
                None => continue,
            };

            for line in output {
                let _ = writeln!(stdout, "{}", line);
            }
            let _ = stdout.flush();
        }
    }

    fn handle_initialize(&self, id: u64) -> String {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({
                "protocolVersion": 1,
                "agentInfo": {
                    "name": "openab-agent",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "agentCapabilities": {
                    "streaming": false,
                    "loadSession": false
                }
            })),
            error: None,
        };
        serde_json::to_string(&resp).unwrap()
    }

    async fn handle_session_new(&mut self, id: u64) -> String {
        let session_id = Uuid::new_v4().to_string();

        // Use struct config if set, then env, then auto-detect
        let provider_choice = self
            .active_provider
            .clone()
            .or_else(|| std::env::var("OPENAB_AGENT_PROVIDER").ok())
            .unwrap_or_default();
        let model_override = self.active_model.as_deref();
        let (provider, active_provider): (Box<dyn crate::llm::LlmProvider>, &str) =
            match provider_choice.as_str() {
                "anthropic" => {
                    let res = match model_override {
                        Some(m) => AnthropicProvider::from_env_with_model(m),
                        None => AnthropicProvider::from_env(),
                    };
                    match res {
                        Ok(p) => (Box::new(p), "anthropic"),
                        Err(e) => return self.error_response(id, -32000, &e),
                    }
                }
                "openai" | "codex" => {
                    let res = match model_override {
                        Some(m) => crate::llm::OpenAiProvider::from_auth_store_with_model(m),
                        None => crate::llm::OpenAiProvider::from_auth_store(),
                    };
                    match res {
                        Ok(p) => (Box::new(p), "openai"),
                        Err(e) => return self.error_response(id, -32000, &e),
                    }
                }
                _ => {
                    // Auto-detect: try API key first, then OAuth token
                    let anthropic_res = match model_override {
                        Some(m) => AnthropicProvider::from_env_with_model(m),
                        None => AnthropicProvider::from_env(),
                    };
                    match anthropic_res {
                        Ok(p) => (Box::new(p), "anthropic"),
                        Err(_) => {
                            let openai_res = match model_override {
                                Some(m) => {
                                    crate::llm::OpenAiProvider::from_auth_store_with_model(m)
                                }
                                None => crate::llm::OpenAiProvider::from_auth_store(),
                            };
                            match openai_res {
                                Ok(p) => (Box::new(p), "openai"),
                                Err(e) => {
                                    return self.error_response(
                                        id,
                                        -32000,
                                        &format!("No credentials: set ANTHROPIC_API_KEY or run `openab-agent auth codex-oauth`. {e}"),
                                    )
                                }
                            }
                        }
                    }
                }
            };

        let agent = Agent::new_boxed(provider, self.working_dir.clone());
        self.sessions.insert(session_id.clone(), agent);

        let model_name = self
            .active_model
            .clone()
            .or_else(|| std::env::var("OPENAB_AGENT_MODEL").ok())
            .unwrap_or_else(|| {
                if active_provider == "anthropic" {
                    "claude-sonnet-4-20250514".to_string()
                } else {
                    "gpt-4.1-nano".to_string()
                }
            });

        let resp = JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(json!({
                "sessionId": session_id,
                "configOptions": [{
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "enum",
                    "currentValue": model_name,
                    "options": Self::available_models().await
                }]
            })),
            error: None,
        };
        serde_json::to_string(&resp).unwrap()
    }

    /// List available models based on configured credentials.
    /// Queries provider APIs when possible, falls back to known defaults.
    async fn available_models() -> Vec<Value> {
        let mut models = Vec::new();

        // Query Anthropic models if credentials are available
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            match Self::fetch_anthropic_models().await {
                Ok(fetched) => models.extend(fetched),
                Err(_) => {
                    // Fallback to known defaults if API unreachable
                    models.push(json!({"value": "claude-sonnet-4-20250514", "name": "Claude Sonnet 4", "provider": "anthropic"}));
                    models.push(json!({"value": "claude-haiku-4-20250514", "name": "Claude Haiku 4", "provider": "anthropic"}));
                }
            }
        }

        // Query OpenAI models if credentials are available
        if crate::auth::load_tokens().is_ok() {
            match Self::fetch_openai_models().await {
                Ok(fetched) => models.extend(fetched),
                Err(_) => {
                    // Fallback to known defaults if API unreachable
                    models.push(json!({"value": "gpt-4.1-nano", "name": "GPT-4.1 Nano", "provider": "openai"}));
                    models.push(json!({"value": "gpt-4.1-mini", "name": "GPT-4.1 Mini", "provider": "openai"}));
                    models.push(json!({"value": "o4-mini", "name": "o4-mini", "provider": "openai"}));
                }
            }
        }

        if models.is_empty() {
            models.push(json!({"value": "none", "name": "No credentials configured", "provider": "none"}));
        }
        models
    }

    /// Fetch models from Anthropic /v1/models API.
    async fn fetch_anthropic_models() -> Result<Vec<Value>, String> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|e| e.to_string())?;
        let client = reqwest::Client::new();
        let resp = client
            .get("https://api.anthropic.com/v1/models")
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("Anthropic API returned {}", resp.status()));
        }

        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        let mut models = Vec::new();
        if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
            for m in data {
                if let Some(id) = m.get("id").and_then(|v| v.as_str()) {
                    // Only include chat models (claude-*)
                    if id.starts_with("claude") {
                        let display = m
                            .get("display_name")
                            .and_then(|v| v.as_str())
                            .unwrap_or(id);
                        models.push(json!({"value": id, "name": display, "provider": "anthropic"}));
                    }
                }
            }
        }
        if models.is_empty() {
            return Err("No models returned from Anthropic API".to_string());
        }
        Ok(models)
    }

    /// Fetch models from OpenAI-compatible /models endpoint.
    async fn fetch_openai_models() -> Result<Vec<Value>, String> {
        let tokens = crate::auth::load_tokens().map_err(|e| e.to_string())?;
        let base_url = std::env::var("OPENAB_AGENT_OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string());
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("{}/models", base_url))
            .bearer_auth(&tokens.access_token)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("OpenAI API returned {}", resp.status()));
        }

        let body: Value = resp.json().await.map_err(|e| e.to_string())?;
        let mut models = Vec::new();
        // Handle both { "data": [...] } and [...] shapes
        let items = body
            .get("data")
            .and_then(|d| d.as_array())
            .or_else(|| body.as_array());
        if let Some(data) = items {
            for m in data {
                if let Some(id) = m.get("id").and_then(|v| v.as_str()) {
                    let name = m
                        .get("name")
                        .or_else(|| m.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(id);
                    models.push(json!({"value": id, "name": name, "provider": "openai"}));
                }
            }
        }
        if models.is_empty() {
            return Err("No models returned from OpenAI API".to_string());
        }
        Ok(models)
    }

    /// Sync version for use in non-async contexts (e.g. set_config_option validation).
    /// Uses credential detection with known defaults — the async version queries APIs.
    fn available_models_sync() -> Vec<Value> {
        let mut models = Vec::new();
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            models.push(json!({"value": "claude-sonnet-4-20250514", "name": "Claude Sonnet 4", "provider": "anthropic"}));
            models.push(json!({"value": "claude-haiku-4-20250514", "name": "Claude Haiku 4", "provider": "anthropic"}));
        }
        if crate::auth::load_tokens().is_ok() {
            models.push(json!({"value": "gpt-4.1-nano", "name": "GPT-4.1 Nano", "provider": "openai"}));
            models.push(json!({"value": "gpt-4.1-mini", "name": "GPT-4.1 Mini", "provider": "openai"}));
            models.push(json!({"value": "o4-mini", "name": "o4-mini", "provider": "openai"}));
        }
        if models.is_empty() {
            models.push(json!({"value": "none", "name": "No credentials configured", "provider": "none"}));
        }
        models
    }

    async fn handle_session_prompt(&mut self, id: u64, params: &Value) -> Vec<String> {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let prompt_text = params
            .get("prompt")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        if prompt_text.trim().is_empty() {
            return vec![self.error_response(id, -32602, "prompt is empty")];
        }

        let agent = match self.sessions.get_mut(session_id) {
            Some(a) => a,
            None => {
                return vec![self.error_response(id, -32600, "unknown session")];
            }
        };

        let mut output_lines = Vec::new();
        let session_id_owned = session_id.to_string();

        match agent.run(&prompt_text).await {
            Ok(response_text) => {
                let notification = serde_json::to_string(&JsonRpcNotification {
                    jsonrpc: "2.0",
                    method: "session/update".to_string(),
                    params: json!({
                        "sessionId": session_id_owned,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": response_text }
                        }
                    }),
                })
                .unwrap();
                output_lines.push(notification);
                output_lines.push(self.ok_response(id, json!({ "stopReason": "end_turn" })));
            }
            Err(e) => {
                output_lines.push(self.error_response(id, -32000, &format!("agent error: {e}")));
            }
        }

        output_lines
    }

    fn handle_set_config_option(&mut self, id: u64, params: &Value) -> String {
        let config_id = params
            .get("configId")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let value = params.get("value").and_then(|v| v.as_str()).unwrap_or("");
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if config_id != "model" || value.is_empty() {
            return self.error_response(id, -32602, "unsupported configId or empty value");
        }

        // We need the cached model list; use sync fallback for validation
        let models = Self::available_models_sync();
        let matched = models
            .iter()
            .find(|m| m.get("value").and_then(|v| v.as_str()) == Some(value));
        if matched.is_none() {
            return self.error_response(
                id,
                -32602,
                &format!("unknown model: {value}. Use one from available_models."),
            );
        }

        // Determine provider from model metadata (not name prefix)
        let provider_name = matched
            .unwrap()
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("openai");

        // Store in struct (safe — no env mutation)
        self.active_model = Some(value.to_string());
        self.active_provider = Some(provider_name.to_string());

        // Rebuild the current session's provider so the switch takes effect immediately
        if !session_id.is_empty() && self.sessions.contains_key(session_id) {
            let new_provider: Result<Box<dyn crate::llm::LlmProvider>, String> = match provider_name
            {
                "anthropic" => {
                    AnthropicProvider::from_env_with_model(value).map(|p| Box::new(p) as _)
                }
                _ => crate::llm::OpenAiProvider::from_auth_store_with_model(value)
                    .map(|p| Box::new(p) as _),
            };
            match new_provider {
                Ok(p) => {
                    // Atomic: only remove old session after new provider succeeds
                    self.sessions.remove(session_id);
                    let agent = Agent::new_boxed(p, self.working_dir.clone());
                    self.sessions.insert(session_id.to_string(), agent);
                }
                Err(e) => {
                    return self.error_response(
                        id,
                        -32000,
                        &format!("failed to switch provider: {e}"),
                    );
                }
            }
        }

        self.ok_response(
            id,
            json!({
                "configOptions": [{
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "enum",
                    "currentValue": value,
                    "options": models
                }]
            }),
        )
    }

    fn ok_response(&self, id: u64, result: Value) -> String {
        serde_json::to_string(&JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        })
        .unwrap()
    }

    fn error_response(&self, id: u64, code: i64, message: &str) -> String {
        serde_json::to_string(&JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(json!({ "code": code, "message": message })),
        })
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initialize_response() {
        let server = AcpServer::new();
        let resp_str = server.handle_initialize(1);
        let resp: Value = serde_json::from_str(&resp_str).unwrap();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["agentInfo"]["name"], "openab-agent");
        assert_eq!(resp["result"]["agentCapabilities"]["streaming"], false);
    }

    #[tokio::test]
    async fn test_session_new() {
        // Set a fake key so from_env() succeeds in CI
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "test-key") };
        let mut server = AcpServer::new();
        let resp_str = server.handle_session_new(2).await;
        let resp: Value = serde_json::from_str(&resp_str).unwrap();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 2);
        assert!(resp["result"]["sessionId"].as_str().unwrap().len() > 0);
        // Verify configOptions are returned for /models support
        let config_options = resp["result"]["configOptions"].as_array().unwrap();
        assert!(!config_options.is_empty());
        assert_eq!(config_options[0]["id"], "model");
        assert_eq!(config_options[0]["category"], "model");
        assert!(!config_options[0]["options"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_session_new_missing_key() {
        // Ensure no OAuth token exists either
        let auth_path =
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
                .join(".openab/agent/auth.json");
        let _ = std::fs::remove_file(&auth_path);
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
        let mut server = AcpServer::new();
        let resp_str = server.handle_session_new(3).await;
        let resp: Value = serde_json::from_str(&resp_str).unwrap();
        assert!(resp["error"].is_object());
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ANTHROPIC_API_KEY"));
    }
}

use crate::agent::Agent;
use crate::llm::AnthropicProvider;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

const MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ModelOption {
    value: String,
    name: String,
    provider: String,
}

impl ModelOption {
    fn new(value: &str, name: &str, provider: &str) -> Self {
        Self {
            value: value.to_string(),
            name: name.to_string(),
            provider: provider.to_string(),
        }
    }
}

pub struct AcpServer {
    // TODO(v0.2): add session TTL and periodic cleanup to prevent OOM
    sessions: HashMap<String, Agent>,
    working_dir: String,
    /// Active model name (safe alternative to env mutation)
    active_model: Option<String>,
    /// Active provider name: "anthropic" or "openai" (safe alternative to env mutation)
    active_provider: Option<String>,
    /// Last model list exposed to the ACP client; used to validate model switches.
    model_options: Vec<ModelOption>,
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
            model_options: Vec::new(),
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
        self.model_options = Self::available_models().await;

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
                    "options": self.model_options
                }]
            })),
            error: None,
        };
        serde_json::to_string(&resp).unwrap()
    }

    /// List available models based on configured credentials.
    /// Queries provider APIs when possible, falls back to known defaults.
    async fn available_models() -> Vec<ModelOption> {
        let mut models = Vec::new();

        // Query Anthropic models if credentials are available
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            match Self::fetch_anthropic_models().await {
                Ok(fetched) => models.extend(fetched),
                Err(_) => models.extend(Self::fallback_anthropic_models()),
            }
        }

        // Query OpenAI models if credentials are available
        if crate::auth::load_tokens().is_ok() {
            match Self::fetch_openai_models().await {
                Ok(fetched) => models.extend(fetched),
                Err(_) => models.extend(Self::fallback_openai_models()),
            }
        }

        if models.is_empty() {
            models.push(ModelOption::new(
                "none",
                "No credentials configured",
                "none",
            ));
        }
        models
    }

    /// Fetch models from Anthropic /v1/models API.
    async fn fetch_anthropic_models() -> Result<Vec<ModelOption>, String> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|e| e.to_string())?;
        let client = reqwest::Client::builder()
            .timeout(MODEL_DISCOVERY_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
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
                        let display = m.get("display_name").and_then(|v| v.as_str()).unwrap_or(id);
                        models.push(ModelOption::new(id, display, "anthropic"));
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
    async fn fetch_openai_models() -> Result<Vec<ModelOption>, String> {
        let tokens = crate::auth::load_tokens().map_err(|e| e.to_string())?;
        let base_url = std::env::var("OPENAB_AGENT_OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string());
        let client = reqwest::Client::builder()
            .timeout(MODEL_DISCOVERY_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
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
                    models.push(ModelOption::new(id, name, "openai"));
                }
            }
        }
        if models.is_empty() {
            return Err("No models returned from OpenAI API".to_string());
        }
        Ok(models)
    }

    fn fallback_available_models() -> Vec<ModelOption> {
        let mut models = Vec::new();
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            models.extend(Self::fallback_anthropic_models());
        }
        if crate::auth::load_tokens().is_ok() {
            models.extend(Self::fallback_openai_models());
        }
        if models.is_empty() {
            models.push(ModelOption::new(
                "none",
                "No credentials configured",
                "none",
            ));
        }
        models
    }

    fn fallback_anthropic_models() -> Vec<ModelOption> {
        vec![
            ModelOption::new("claude-sonnet-4-20250514", "Claude Sonnet 4", "anthropic"),
            ModelOption::new("claude-haiku-4-20250514", "Claude Haiku 4", "anthropic"),
        ]
    }

    fn fallback_openai_models() -> Vec<ModelOption> {
        vec![
            ModelOption::new("gpt-4.1-nano", "GPT-4.1 Nano", "openai"),
            ModelOption::new("gpt-4.1-mini", "GPT-4.1 Mini", "openai"),
            ModelOption::new("o4-mini", "o4-mini", "openai"),
        ]
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

        let models = if self.model_options.is_empty() {
            Self::fallback_available_models()
        } else {
            self.model_options.clone()
        };
        let matched = models
            .iter()
            .find(|m| m.value == value)
            .cloned()
            .ok_or_else(|| format!("unknown model: {value}. Use one from available_models."));
        let matched = match matched {
            Ok(m) => m,
            Err(e) => return self.error_response(id, -32602, &e),
        };
        let provider_name = matched.provider.as_str();

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
                    // Swap provider in-place, preserving conversation history
                    self.sessions.get_mut(session_id).unwrap().swap_provider(p);
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

        // Update state only after successful rebuild (avoids stale state on failure)
        self.active_model = Some(value.to_string());
        self.active_provider = Some(matched.provider.clone());

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
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
        let _guard = ENV_LOCK.lock().unwrap();
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
        let _guard = ENV_LOCK.lock().unwrap();
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

    #[test]
    fn test_set_config_option_accepts_cached_dynamic_model() {
        let mut server = AcpServer::new();
        server.model_options = vec![ModelOption::new(
            "claude-opus-4-20250514",
            "Claude Opus 4",
            "anthropic",
        )];

        let resp_str = server.handle_set_config_option(
            4,
            &json!({
                "configId": "model",
                "value": "claude-opus-4-20250514",
            }),
        );
        let resp: Value = serde_json::from_str(&resp_str).unwrap();

        assert!(resp["error"].is_null());
        assert_eq!(
            resp["result"]["configOptions"][0]["currentValue"],
            "claude-opus-4-20250514"
        );
        assert_eq!(
            server.active_model.as_deref(),
            Some("claude-opus-4-20250514")
        );
        assert_eq!(server.active_provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn test_set_config_option_rejects_unknown_model() {
        let mut server = AcpServer::new();
        server.model_options = vec![ModelOption::new(
            "claude-opus-4-20250514",
            "Claude Opus 4",
            "anthropic",
        )];

        let resp_str = server.handle_set_config_option(
            5,
            &json!({
                "configId": "model",
                "value": "not-in-menu",
            }),
        );
        let resp: Value = serde_json::from_str(&resp_str).unwrap();

        assert_eq!(resp["error"]["code"], -32602);
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown model"));
    }

    #[tokio::test]
    async fn test_model_switch_preserves_session_history() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "test-key") };
        let mut server = AcpServer::new();

        // Create a session
        let resp_str = server.handle_session_new(10).await;
        let resp: Value = serde_json::from_str(&resp_str).unwrap();
        let session_id = resp["result"]["sessionId"].as_str().unwrap().to_string();

        // Simulate conversation history by pushing a message into the agent
        let agent = server.sessions.get_mut(&session_id).unwrap();
        agent.messages.push(crate::llm::Message {
            role: "user".to_string(),
            content: vec![crate::llm::ContentBlock::Text {
                text: "hello".to_string(),
            }],
        });
        assert_eq!(agent.message_count(), 1);

        // Switch model (same provider — should succeed with test-key)
        server.model_options = vec![
            ModelOption::new("claude-sonnet-4-20250514", "Claude Sonnet 4", "anthropic"),
            ModelOption::new("claude-haiku-4-20250514", "Claude Haiku 4", "anthropic"),
        ];
        let resp_str = server.handle_set_config_option(
            11,
            &json!({
                "configId": "model",
                "value": "claude-haiku-4-20250514",
                "sessionId": session_id,
            }),
        );
        let resp: Value = serde_json::from_str(&resp_str).unwrap();
        assert!(resp["error"].is_null(), "switch should succeed");

        // Verify conversation history is preserved
        let agent = server.sessions.get(&session_id).unwrap();
        assert_eq!(
            agent.message_count(),
            1,
            "messages must survive model switch"
        );
    }

    #[test]
    fn test_failed_switch_does_not_update_state() {
        let _guard = ENV_LOCK.lock().unwrap();
        let mut server = AcpServer::new();
        server.model_options = vec![ModelOption::new("gpt-4.1-nano", "GPT-4.1 Nano", "openai")];

        // No OpenAI credentials → rebuild will fail
        let resp_str = server.handle_set_config_option(
            12,
            &json!({
                "configId": "model",
                "value": "gpt-4.1-nano",
                "sessionId": "nonexistent-session",
            }),
        );
        let resp: Value = serde_json::from_str(&resp_str).unwrap();

        // No session exists so it skips rebuild, state still updates
        // (the guard only fires when session exists)
        assert!(resp["error"].is_null());

        // Now test with a real session that will fail on rebuild
        // Reset state
        server.active_model = None;
        server.active_provider = None;

        // Insert a dummy session using anthropic key
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "test-key") };
        let provider = AnthropicProvider::from_env_with_model("claude-sonnet-4-20250514").unwrap();
        let agent = Agent::new_boxed(Box::new(provider), "/tmp".to_string());
        server.sessions.insert("test-session".to_string(), agent);

        // Remove anthropic key and try to switch to anthropic model → should fail
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
        server.model_options = vec![ModelOption::new(
            "claude-opus-4-20250514",
            "Claude Opus 4",
            "anthropic",
        )];
        let resp_str = server.handle_set_config_option(
            13,
            &json!({
                "configId": "model",
                "value": "claude-opus-4-20250514",
                "sessionId": "test-session",
            }),
        );
        let resp: Value = serde_json::from_str(&resp_str).unwrap();

        assert!(resp["error"].is_object(), "rebuild should fail");
        // State should NOT have been updated
        assert_eq!(
            server.active_model, None,
            "active_model must not change on failure"
        );
        assert_eq!(
            server.active_provider, None,
            "active_provider must not change on failure"
        );
    }
}

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::pin::Pin;

/// A message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

/// A content block within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

/// Tool definition sent to the LLM.
#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Events streamed back from the LLM.
#[derive(Debug, Clone)]
pub enum LlmEvent {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Stop,
    #[allow(dead_code)]
    Error(String),
}

/// Trait for LLM providers.
pub trait LlmProvider: Send + Sync {
    fn chat<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<LlmEvent>>> + Send + 'a>>;
}

/// Anthropic Claude provider.
pub struct AnthropicProvider {
    api_key: String,
    model: String,
    max_tokens: u32,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn from_env() -> Result<Self, String> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| "ANTHROPIC_API_KEY not set".to_string())?;
        if api_key.is_empty() {
            return Err("ANTHROPIC_API_KEY is empty".to_string());
        }
        Ok(Self {
            api_key,
            model: std::env::var("OPENAB_AGENT_MODEL")
                .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string()),
            max_tokens: std::env::var("OPENAB_AGENT_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8192),
            client: reqwest::Client::new(),
        })
    }

    fn build_request_body(&self, system: &str, messages: &[Message], tools: &[ToolDef]) -> Value {
        let msgs: Vec<Value> = messages
            .iter()
            .map(|m| {
                let content: Vec<Value> = m
                    .content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
                        ContentBlock::ToolUse { id, name, input } => {
                            json!({ "type": "tool_use", "id": id, "name": name, "input": input })
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let mut v = json!({
                                "type": "tool_result",
                                "tool_use_id": tool_use_id,
                                "content": content
                            });
                            if let Some(true) = is_error {
                                v["is_error"] = json!(true);
                            }
                            v
                        }
                    })
                    .collect();
                json!({ "role": &m.role, "content": content })
            })
            .collect();

        let mut body = json!({
            "model": &self.model,
            "max_tokens": self.max_tokens,
            "messages": msgs,
            "system": system,
        });

        if !tools.is_empty() {
            let tool_defs: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "name": &t.name,
                        "description": &t.description,
                        "input_schema": &t.input_schema
                    })
                })
                .collect();
            body["tools"] = json!(tool_defs);
        }

        body
    }
}

impl LlmProvider for AnthropicProvider {
    fn chat<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<LlmEvent>>> + Send + 'a>> {
        Box::pin(async move {
            let body = self.build_request_body(system, messages, tools);
            let max_retries = 3u32;

            for attempt in 0..=max_retries {
                let resp = self
                    .client
                    .post("https://api.anthropic.com/v1/messages")
                    .header("x-api-key", &self.api_key)
                    .header("anthropic-version", "2023-06-01")
                    .header("content-type", "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| anyhow!("HTTP request failed: {e}"))?;

                let status = resp.status();

                // Retry on 429 (rate limit) or 529 (overloaded)
                if (status.as_u16() == 429 || status.as_u16() == 529) && attempt < max_retries {
                    let delay = std::time::Duration::from_millis(1000 * 2u64.pow(attempt));
                    tokio::time::sleep(delay).await;
                    continue;
                }

                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("Anthropic API error {status}: {text}"));
                }

                let response: Value = resp
                    .json()
                    .await
                    .map_err(|e| anyhow!("Failed to parse response: {e}"))?;

                return parse_anthropic_response(&response);
            }

            Err(anyhow!("Anthropic API: max retries exceeded"))
        })
    }
}

fn parse_anthropic_response(response: &Value) -> Result<Vec<LlmEvent>> {
    let mut events = Vec::new();

    let content = response
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("missing content in response"))?;

    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    events.push(LlmEvent::Text(text.to_string()));
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(json!({}));
                events.push(LlmEvent::ToolUse { id, name, input });
            }
            _ => {}
        }
    }

    let stop_reason = response
        .get("stop_reason")
        .and_then(|s| s.as_str())
        .unwrap_or("end_turn");

    if stop_reason != "tool_use" {
        events.push(LlmEvent::Stop);
    }

    Ok(events)
}

// === OpenAI-compatible Provider (for Codex subscription via OAuth) ===

pub struct OpenAiProvider {
    base_url: String,
    model: String,
    max_tokens: u32,
    client: reqwest::Client,
}

impl OpenAiProvider {
    /// Create provider using stored OAuth token from ~/.openab/agent/auth.json
    pub fn from_auth_store() -> Result<Self, String> {
        // Just verify tokens exist; actual token is fetched at call time
        crate::auth::load_tokens().map_err(|e| e.to_string())?;
        Ok(Self {
            base_url: std::env::var("OPENAB_AGENT_OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com".to_string()),
            model: std::env::var("OPENAB_AGENT_OPENAI_MODEL")
                .or_else(|_| std::env::var("OPENAB_AGENT_MODEL"))
                .unwrap_or_else(|_| "gpt-4o".to_string()),
            max_tokens: std::env::var("OPENAB_AGENT_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8192),
            client: reqwest::Client::new(),
        })
    }
}

impl LlmProvider for OpenAiProvider {
    fn chat<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<LlmEvent>>> + Send + 'a>> {
        Box::pin(async move {
            // Build OpenAI-format messages
            let mut oai_messages: Vec<Value> = vec![json!({"role": "system", "content": system})];
            for m in messages {
                // OpenAI uses different message format for tool results
                if m.role == "user"
                    && m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
                {
                    for b in &m.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } = b
                        {
                            oai_messages.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content
                            }));
                        }
                    }
                } else if m.role == "assistant"
                    && m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
                {
                    let mut tool_calls = Vec::new();
                    let mut text_content = String::new();
                    for b in &m.content {
                        match b {
                            ContentBlock::ToolUse { id, name, input } => {
                                tool_calls.push(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {"name": name, "arguments": input.to_string()}
                                }));
                            }
                            ContentBlock::Text { text } => text_content.push_str(text),
                            _ => {}
                        }
                    }
                    let mut msg = json!({"role": "assistant"});
                    if !text_content.is_empty() {
                        msg["content"] = json!(text_content);
                    }
                    if !tool_calls.is_empty() {
                        msg["tool_calls"] = json!(tool_calls);
                    }
                    oai_messages.push(msg);
                } else {
                    let text: String = m
                        .content
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::Text { text } = b {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    oai_messages.push(json!({"role": &m.role, "content": text}));
                }
            }

            let mut body = json!({
                "model": &self.model,
                "max_tokens": self.max_tokens,
                "messages": oai_messages,
            });

            if !tools.is_empty() {
                let oai_tools: Vec<Value> = tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": &t.name,
                                "description": &t.description,
                                "parameters": &t.input_schema
                            }
                        })
                    })
                    .collect();
                body["tools"] = json!(oai_tools);
            }

            let max_retries = 3u32;
            for attempt in 0..=max_retries {
                let token = crate::auth::get_valid_token().await?;
                let resp = self
                    .client
                    .post(format!("{}/v1/chat/completions", self.base_url))
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| anyhow!("HTTP request failed: {e}"))?;

                let status = resp.status();
                if (status.as_u16() == 429 || status.as_u16() == 529) && attempt < max_retries {
                    let delay = std::time::Duration::from_millis(1000 * 2u64.pow(attempt));
                    tokio::time::sleep(delay).await;
                    continue;
                }

                // 401: token may have expired mid-request, force refresh and retry
                if status.as_u16() == 401 && attempt < max_retries {
                    let _ = crate::auth::force_refresh().await;
                    continue;
                }

                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("OpenAI API error {status}: {text}"));
                }

                let response: Value = resp
                    .json()
                    .await
                    .map_err(|e| anyhow!("Failed to parse response: {e}"))?;

                return parse_openai_response(&response);
            }
            Err(anyhow!("OpenAI API: max retries exceeded"))
        })
    }
}

fn parse_openai_response(response: &Value) -> Result<Vec<LlmEvent>> {
    let mut events = Vec::new();

    let choice = response
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow!("No choices in response"))?;

    let message = choice.get("message").ok_or_else(|| anyhow!("No message"))?;

    // Text content
    if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
        if !content.is_empty() {
            events.push(LlmEvent::Text(content.to_string()));
        }
    }

    // Tool calls
    if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args_str = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
            events.push(LlmEvent::ToolUse { id, name, input });
        }
    }

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop");
    if finish_reason != "tool_calls" {
        events.push(LlmEvent::Stop);
    }

    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_text_response() {
        let resp = json!({
            "content": [{"type": "text", "text": "Hello world"}],
            "stop_reason": "end_turn"
        });
        let events = parse_anthropic_response(&resp).unwrap();
        assert_eq!(events.len(), 2);
        match &events[0] {
            LlmEvent::Text(t) => assert_eq!(t, "Hello world"),
            _ => panic!("expected Text event"),
        }
        assert!(matches!(events[1], LlmEvent::Stop));
    }

    #[test]
    fn test_parse_tool_use_response() {
        let resp = json!({
            "content": [
                {"type": "tool_use", "id": "tu_1", "name": "read", "input": {"path": "/tmp/x"}}
            ],
            "stop_reason": "tool_use"
        });
        let events = parse_anthropic_response(&resp).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "tu_1");
                assert_eq!(name, "read");
                assert_eq!(input["path"], "/tmp/x");
            }
            _ => panic!("expected ToolUse event"),
        }
    }

    #[test]
    fn test_build_request_body() {
        let provider = AnthropicProvider {
            api_key: "test".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 4096,
            client: reqwest::Client::new(),
        };
        let messages = vec![Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
        }];
        let body = provider.build_request_body("system prompt", &messages, &[]);
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["system"], "system prompt");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn test_parse_openai_text_response() {
        let resp = json!({
            "choices": [{"message": {"content": "Hello"}, "finish_reason": "stop"}]
        });
        let events = parse_openai_response(&resp).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], LlmEvent::Text(t) if t == "Hello"));
        assert!(matches!(events[1], LlmEvent::Stop));
    }

    #[test]
    fn test_parse_openai_tool_call_response() {
        let resp = json!({
            "choices": [{"message": {
                "content": null,
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"x.txt\"}"}}]
            }, "finish_reason": "tool_calls"}]
        });
        let events = parse_openai_response(&resp).unwrap();
        assert_eq!(events.len(), 1);
        match &events[0] {
            LlmEvent::ToolUse { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "read");
                assert_eq!(input["path"], "x.txt");
            }
            _ => panic!("expected ToolUse"),
        }
    }

    #[test]
    fn test_parse_openai_empty_choices() {
        let resp = json!({"choices": []});
        assert!(parse_openai_response(&resp).is_err());
    }
}

use anyhow::{anyhow, Result};
use base64::Engine;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::pin::Pin;
use tokio::io::AsyncBufReadExt;
use tokio_util::io::StreamReader;

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

/// Callback invoked for each text chunk during streaming.
/// Pass `&dyn Fn(&str)` directly to avoid double-indirection through Box.
pub type TextCallback = dyn Fn(&str) + Send + Sync;

/// Trait for LLM providers.
pub trait LlmProvider: Send + Sync {
    fn chat<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolDef],
        on_text: Option<&'a TextCallback>,
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
            "stream": true,
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
        on_text: Option<&'a TextCallback>,
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

                if (status.as_u16() == 429 || status.as_u16() == 529) && attempt < max_retries {
                    let delay = std::time::Duration::from_millis(1000 * 2u64.pow(attempt));
                    tokio::time::sleep(delay).await;
                    continue;
                }

                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("Anthropic API error {status}: {text}"));
                }

                // Parse SSE stream
                let byte_stream = resp.bytes_stream();
                let stream_reader = StreamReader::new(
                    byte_stream
                        .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))),
                );
                let mut lines = tokio::io::BufReader::new(stream_reader).lines();

                let mut events = Vec::new();
                let mut current_text = String::new();
                let mut tool_id = String::new();
                let mut tool_name = String::new();
                let mut tool_input_json = String::new();
                let mut in_tool_use = false;
                let mut stop_reason = String::new();

                while let Some(line) = lines
                    .next_line()
                    .await
                    .map_err(|e| anyhow!("stream read: {e}"))?
                {
                    let line = line.trim().to_string();
                    if !line.starts_with("data: ") {
                        continue;
                    }
                    let data = &line[6..];
                    if data == "[DONE]" {
                        break;
                    }
                    let event: Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

                    match event_type {
                        "content_block_start" => {
                            let block = &event["content_block"];
                            if let Some("tool_use") = block.get("type").and_then(|t| t.as_str()) {
                                // Flush any accumulated text
                                if !current_text.is_empty() {
                                    events.push(LlmEvent::Text(current_text.clone()));
                                    current_text.clear();
                                }
                                in_tool_use = true;
                                tool_id = block["id"].as_str().unwrap_or("").to_string();
                                tool_name = block["name"].as_str().unwrap_or("").to_string();
                                tool_input_json.clear();
                            }
                        }
                        "content_block_delta" => {
                            let delta = &event["delta"];
                            match delta.get("type").and_then(|t| t.as_str()) {
                                Some("text_delta") => {
                                    if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                                        current_text.push_str(text);
                                        if let Some(cb) = on_text {
                                            cb(text);
                                        }
                                    }
                                }
                                Some("input_json_delta") => {
                                    if let Some(json_chunk) =
                                        delta.get("partial_json").and_then(|t| t.as_str())
                                    {
                                        tool_input_json.push_str(json_chunk);
                                    }
                                }
                                _ => {}
                            }
                        }
                        "content_block_stop" => {
                            if in_tool_use {
                                let input: Value =
                                    serde_json::from_str(&tool_input_json).unwrap_or(json!({}));
                                events.push(LlmEvent::ToolUse {
                                    id: tool_id.clone(),
                                    name: tool_name.clone(),
                                    input,
                                });
                                in_tool_use = false;
                            } else if !current_text.is_empty() {
                                events.push(LlmEvent::Text(current_text.clone()));
                                current_text.clear();
                            }
                        }
                        "message_delta" => {
                            if let Some(sr) =
                                event["delta"].get("stop_reason").and_then(|s| s.as_str())
                            {
                                stop_reason = sr.to_string();
                            }
                        }
                        "error" => {
                            let msg = event["error"]["message"]
                                .as_str()
                                .unwrap_or("unknown stream error");
                            return Err(anyhow!("Anthropic stream error: {msg}"));
                        }
                        _ => {} // message_start, ping, etc. — no action needed
                    }
                }

                // Flush remaining text
                if !current_text.is_empty() {
                    events.push(LlmEvent::Text(current_text));
                }

                if stop_reason != "tool_use" {
                    events.push(LlmEvent::Stop);
                }

                return Ok(events);
            }

            Err(anyhow!("Anthropic API: max retries exceeded"))
        })
    }
}

// === OpenAI-compatible Provider (for Codex subscription via OAuth) ===

pub struct OpenAiProvider {
    base_url: String,
    model: String,
    #[allow(dead_code)]
    max_tokens: u32,
    client: reqwest::Client,
}

impl OpenAiProvider {
    /// Create provider using stored OAuth token from ~/.openab/agent/auth.json
    pub fn from_auth_store() -> Result<Self, String> {
        crate::auth::load_tokens().map_err(|e| e.to_string())?;
        Ok(Self {
            base_url: std::env::var("OPENAB_AGENT_OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string()),
            model: std::env::var("OPENAB_AGENT_OPENAI_MODEL")
                .or_else(|_| std::env::var("OPENAB_AGENT_MODEL"))
                .unwrap_or_else(|_| "gpt-4.1-nano".to_string()),
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
        on_text: Option<&'a TextCallback>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<LlmEvent>>> + Send + 'a>> {
        Box::pin(async move {
            // Build Responses API input format
            let mut oai_messages: Vec<Value> = vec![];
            for m in messages {
                if m.role == "user" {
                    let texts: Vec<&str> = m
                        .content
                        .iter()
                        .filter_map(|b| {
                            if let ContentBlock::Text { text } = b {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !texts.is_empty() {
                        oai_messages.push(json!({"role": "user", "content": [{"type": "input_text", "text": texts.join("")}]}));
                    }
                    for b in &m.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } = b
                        {
                            oai_messages.push(json!({"type": "function_call_output", "call_id": tool_use_id, "output": content}));
                        }
                    }
                } else if m.role == "assistant" {
                    for b in &m.content {
                        match b {
                            ContentBlock::Text { text } => {
                                oai_messages.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]}));
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                oai_messages.push(json!({"type": "function_call", "call_id": id, "name": name, "arguments": input.to_string()}));
                            }
                            _ => {}
                        }
                    }
                }
            }

            let mut body = json!({
                "model": &self.model,
                "store": false,
                "stream": true,
                "instructions": system,
                "input": oai_messages,
                "tool_choice": "auto",
                "parallel_tool_calls": true,
            });

            if !tools.is_empty() {
                let resp_tools: Vec<Value> = tools
                    .iter()
                    .map(|t| {
                        json!({
                            "type": "function",
                            "name": &t.name,
                            "description": &t.description,
                            "parameters": &t.input_schema
                        })
                    })
                    .collect();
                body["tools"] = json!(resp_tools);
            }

            let max_retries = 3u32;
            for attempt in 0..=max_retries {
                let token = crate::auth::get_valid_token().await?;
                let account_id = extract_account_id_from_jwt(&token);
                let mut req = self
                    .client
                    .post(format!("{}/codex/responses", self.base_url))
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .header("originator", "openab-agent");
                if let Some(ref aid) = account_id {
                    req = req.header("chatgpt-account-id", aid);
                }
                let resp = req
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

                if status.as_u16() == 401 && attempt < max_retries {
                    let _ = crate::auth::force_refresh().await;
                    continue;
                }

                if !status.is_success() {
                    let text = resp.text().await.unwrap_or_default();
                    return Err(anyhow!("OpenAI API error {status}: {text}"));
                }

                // Stream SSE line-by-line
                let byte_stream = resp.bytes_stream();
                let stream_reader = StreamReader::new(
                    byte_stream
                        .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))),
                );
                let mut lines = tokio::io::BufReader::new(stream_reader).lines();

                let mut output_items: Vec<Value> = Vec::new();
                let mut current_text = String::new();

                while let Some(line) = lines
                    .next_line()
                    .await
                    .map_err(|e| anyhow!("stream read: {e}"))?
                {
                    let line = line.trim().to_string();
                    if let Some(data) = line.strip_prefix("data: ") {
                        if data == "[DONE]" {
                            break;
                        }
                        if let Ok(event) = serde_json::from_str::<Value>(data) {
                            let event_type =
                                event.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            match event_type {
                                "response.output_text.delta" => {
                                    if let Some(delta) = event.get("delta").and_then(|d| d.as_str())
                                    {
                                        current_text.push_str(delta);
                                        if let Some(cb) = on_text {
                                            cb(delta);
                                        }
                                    }
                                }
                                "response.output_item.done" => {
                                    if let Some(item) = event.get("item") {
                                        output_items.push(item.clone());
                                    }
                                }
                                "error" => {
                                    let msg = event["error"]["message"]
                                        .as_str()
                                        .or_else(|| event.get("message").and_then(|m| m.as_str()))
                                        .unwrap_or("unknown stream error");
                                    return Err(anyhow!("OpenAI stream error: {msg}"));
                                }
                                _ => {} // response.created, response.in_progress, etc.
                            }
                        }
                    }
                }

                if output_items.is_empty() && current_text.is_empty() {
                    return Err(anyhow!("No output in SSE stream"));
                }

                // If we collected output_items, parse them (includes function_calls)
                if !output_items.is_empty() {
                    let response = json!({"output": output_items});
                    return parse_openai_response(&response);
                }

                // Fallback: text-only response
                return Ok(vec![LlmEvent::Text(current_text), LlmEvent::Stop]);
            }
            Err(anyhow!("OpenAI API: max retries exceeded"))
        })
    }
}

fn extract_account_id_from_jwt(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut payload = parts[1].to_string();
    while !payload.len().is_multiple_of(4) {
        payload.push('=');
    }
    let decoded = base64::engine::general_purpose::URL_SAFE
        .decode(&payload)
        .ok()
        .or_else(|| {
            base64::engine::general_purpose::STANDARD
                .decode(&payload)
                .ok()
        })?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .map(|s| s.to_string())
}

fn parse_openai_response(response: &Value) -> Result<Vec<LlmEvent>> {
    let mut events = Vec::new();

    // Handle Responses API format (output array)
    if let Some(output) = response.get("output").and_then(|o| o.as_array()) {
        for item in output {
            match item.get("type").and_then(|t| t.as_str()) {
                Some("message") => {
                    if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                        for block in content {
                            if block.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    events.push(LlmEvent::Text(text.to_string()));
                                }
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args_str = item
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}");
                    let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
                    events.push(LlmEvent::ToolUse { id, name, input });
                }
                _ => {}
            }
        }
        events.push(LlmEvent::Stop);
        return Ok(events);
    }

    // Fallback: Chat Completions format
    let choice = response
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow!("No choices in response"))?;

    let message = choice.get("message").ok_or_else(|| anyhow!("No message"))?;

    if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
        if !content.is_empty() {
            events.push(LlmEvent::Text(content.to_string()));
        }
    }

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

    #[test]
    fn test_parse_openai_responses_api_format() {
        let resp = json!({
            "output": [
                {"type": "message", "content": [{"type": "output_text", "text": "Hi"}]},
            ]
        });
        let events = parse_openai_response(&resp).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], LlmEvent::Text(t) if t == "Hi"));
        assert!(matches!(events[1], LlmEvent::Stop));
    }

    #[test]
    fn test_build_request_body_has_stream() {
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
        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
    }
}

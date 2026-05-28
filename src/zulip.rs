//! Zulip adapter (Phase Zulip — multi-platform).
//!
//! Implements `ChatAdapter` for Zulip via the REST API: send/edit messages,
//! react with unicode emoji, long-poll the events API, and recover transparently
//! from `BAD_EVENT_QUEUE_ID` (queue expiry / server restart).
//!
//! Authentication: HTTP Basic auth with `bot_email:api_key`. The bot **must** be
//! a generic-bot account; outgoing-webhook bots cannot edit messages or react.
//!
//! Thread/session key shape (see ADR §11 — Phase Zulip):
//! - Stream messages: `zulip:stream:{stream_id}:{normalized_topic}` (topic
//!   lowercased + trimmed). Topic rename forks the session — documented limit.
//! - DMs: `zulip:dm:{sorted_csv_of_user_ids}`.

use crate::adapter::{ChannelRef, ChatAdapter, MessageRef};
use crate::config::{AllowBots, AllowUsers, SttConfig};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::watch;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Pure helpers (unit-testable without HTTP)
// ---------------------------------------------------------------------------

/// Origin of a Zulip message event — used by `thread_key_for_event` to build
/// the ACP session key.
#[derive(Debug, Clone)]
pub enum ZulipEventKind<'a> {
    /// Stream message identified by numeric stream ID + topic name.
    Stream { stream_id: u64, topic: &'a str },
    /// Direct (private) message between a set of user IDs (the bot + recipients).
    Dm { user_ids: &'a [u64] },
}

/// Derive the ACP session/thread key from a Zulip event.
///
/// - Stream → `zulip:stream:{stream_id}:{normalized_topic}` where the topic
///   is trimmed and ASCII-lowercased.
/// - DM → `zulip:dm:{sorted_csv_of_user_ids}`.
///
/// Order-independent for DMs (sorted) and case/whitespace-insensitive for
/// stream topics (so trivial reformatting doesn't fork the session).
pub fn thread_key_for_event(kind: &ZulipEventKind<'_>) -> String {
    match kind {
        ZulipEventKind::Stream { stream_id, topic } => {
            let normalized = topic.trim().to_ascii_lowercase();
            format!("zulip:stream:{stream_id}:{normalized}")
        }
        ZulipEventKind::Dm { user_ids } => {
            let mut ids: Vec<u64> = user_ids.to_vec();
            ids.sort_unstable();
            let csv = ids
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");
            format!("zulip:dm:{csv}")
        }
    }
}

/// Parse an `/eom` invocation. Returns `Some(args)` if `body` starts with
/// the literal `/eom` followed by whitespace or end-of-string; `args` is the
/// trimmed remainder (may be empty). Leading whitespace before `/eom`
/// disqualifies — keeps the rule simple and unambiguous.
pub fn parse_eom(body: &str) -> Option<String> {
    let rest = body.strip_prefix("/eom")?;
    if rest.is_empty() {
        return Some(String::new());
    }
    let next = rest.chars().next().unwrap();
    if !next.is_whitespace() {
        return None;
    }
    Some(rest.trim().to_string())
}

/// Decide whether a Zulip event passes the allowlist gate.
///
/// `stream_id` is `Some(numeric_id_as_str)` for stream messages and `None` for
/// DMs (where channel-level allowlisting is N/A but user-level still applies).
/// `sender_id` is the numeric sender ID as a string. Lists hold numeric IDs as
/// strings to match the schema parity with Slack/Discord.
pub fn allowlist_accepts(
    stream_id: Option<&str>,
    sender_id: &str,
    allow_all_channels: bool,
    allow_all_users: bool,
    allowed_channels: &HashSet<String>,
    allowed_users: &HashSet<String>,
) -> bool {
    if !allow_all_channels {
        if let Some(sid) = stream_id {
            if !allowed_channels.contains(sid) {
                return false;
            }
        }
        // DMs (stream_id == None) bypass the channel check.
    }
    if !allow_all_users && !allowed_users.contains(sender_id) {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// ZulipAdapter
// ---------------------------------------------------------------------------

/// Build the form body for a `/api/v1/typing` request.
///
/// Mirrors `send_message`'s form shape (`zulip.rs` send_message): stream events
/// carry `type=stream`, `to=<stream_id>`, `topic=<topic>`, and `stream_id` for
/// server compatibility; DMs carry `type=direct` with `to=<json-array>` (the
/// recipient list verbatim, as already stored on the ChannelRef).
#[allow(dead_code)] // wired in DispatcherSpawnsTyping; tests exercise directly.
fn typing_form(op: &str, channel: &ChannelRef) -> Vec<(&'static str, String)> {
    let mut form: Vec<(&'static str, String)> = vec![("op", op.to_string())];
    if let Some(topic) = &channel.thread_id {
        form.push(("type", "stream".to_string()));
        form.push(("to", channel.channel_id.clone()));
        form.push(("topic", topic.clone()));
        form.push(("stream_id", channel.channel_id.clone()));
    } else {
        // DM: `type=direct` (Zulip's typing endpoint uses `direct`, not
        // `private`); `to` is the JSON-array literal of recipient IDs.
        form.push(("type", "direct".to_string()));
        form.push(("to", channel.channel_id.clone()));
    }
    form
}

/// Maps the default `[reactions.emojis]` unicode codepoints to Zulip emoji
/// names (the API accepts a CLDR-style short name). Unknown emoji fall back to
/// `question` so a misconfigured custom emoji doesn't break the reaction path.
fn unicode_to_zulip_emoji(unicode: &str) -> &str {
    match unicode {
        "👀" => "eyes",
        "🛠\u{fe0f}" => "working_on_it",
        "🤔" => "thinking",
        "🔥" => "fire",
        "👨\u{200d}💻" => "man_technologist",
        "⚡" => "zap",
        "🆗" => "ok",
        "😱" => "scream",
        "🚫" => "no_entry_sign",
        "😊" => "blush",
        "😎" => "sunglasses",
        "🫡" => "saluting_face",
        "🤓" => "nerd_face",
        "😏" => "smirk",
        "✌\u{fe0f}" => "v",
        "💪" => "muscle",
        "🥱" => "yawning_face",
        "😨" => "fearful",
        "✅" => "white_check_mark",
        "❌" => "x",
        "🔧" => "wrench",
        "🎤" => "microphone",
        _ => "question",
    }
}

/// Zulip adapter — owns the HTTP client and bot credentials.
pub struct ZulipAdapter {
    client: reqwest::Client,
    site: String,
    bot_email: String,
    api_key: String,
}

impl ZulipAdapter {
    /// Construct a new adapter. `site` is the Zulip server base URL
    /// (e.g. `https://your-org.zulipchat.com`) — used as the prefix for all
    /// REST calls. `bot_email` + `api_key` form the HTTP Basic auth pair.
    pub fn new(
        site: impl Into<String>,
        bot_email: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let mut site_str = site.into();
        // Normalize: strip trailing slash so we can paste-concat `/api/v1/...`.
        while site_str.ends_with('/') {
            site_str.pop();
        }
        Self {
            client: reqwest::Client::new(),
            site: site_str,
            bot_email: bot_email.into(),
            api_key: api_key.into(),
        }
    }

    /// Authenticated REST call. Returns the parsed JSON envelope.
    ///
    /// Semantics:
    /// - HTTP 429 → sleep for `Retry-After` seconds (capped at 60s, default 1s)
    ///   then retry once. Further failures propagate.
    /// - HTTP 4xx (non-429) → `Err` carrying the status + the response body's
    ///   `code`/`msg` if present (no retry).
    /// - HTTP 2xx with `result == "error"` → `Err("Zulip <path>: <code>: <msg>")`.
    /// - HTTP 5xx → `Err` (caller decides retry policy; event loop applies
    ///   exponential back-off).
    pub async fn api_call(
        &self,
        method: reqwest::Method,
        path: &str,
        form: Option<&[(&str, String)]>,
        query: Option<&[(&str, String)]>,
    ) -> Result<serde_json::Value> {
        // Try up to twice — second attempt only on 429 with Retry-After.
        let mut retried = false;
        loop {
            let url = format!("{}{}", self.site, path);
            let mut req = self
                .client
                .request(method.clone(), &url)
                .basic_auth(&self.bot_email, Some(&self.api_key));
            if let Some(q) = query {
                req = req.query(q);
            }
            if let Some(f) = form {
                req = req.form(f);
            }
            let resp = req.send().await?;
            let status = resp.status();
            if status.as_u16() == 429 && !retried {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(1)
                    .min(60);
                warn!(path, retry_after, "zulip 429, honoring Retry-After");
                tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
                retried = true;
                continue;
            }
            let body_text = resp.text().await.unwrap_or_default();
            let parsed: serde_json::Value =
                serde_json::from_str(&body_text).unwrap_or(serde_json::Value::Null);
            if !status.is_success() {
                let code = parsed.get("code").and_then(|v| v.as_str()).unwrap_or("");
                let msg = parsed.get("msg").and_then(|v| v.as_str()).unwrap_or("");
                return Err(anyhow!(
                    "Zulip {path}: HTTP {} {}{}",
                    status.as_u16(),
                    if code.is_empty() { "" } else { code },
                    if msg.is_empty() {
                        String::new()
                    } else {
                        format!(": {msg}")
                    }
                ));
            }
            if parsed.get("result").and_then(|v| v.as_str()) == Some("error") {
                let code = parsed
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("UNKNOWN");
                let msg = parsed
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                return Err(anyhow!("Zulip {path}: {code}: {msg}"));
            }
            return Ok(parsed);
        }
    }
}

#[async_trait]
impl ChatAdapter for ZulipAdapter {
    fn platform(&self) -> &'static str {
        "zulip"
    }

    fn message_limit(&self) -> usize {
        // Zulip allows up to 10_000 chars per message; keep parity with Slack's
        // generous limit and let the broker's format module split as needed.
        10_000
    }

    async fn send_message(&self, channel: &ChannelRef, content: &str) -> Result<MessageRef> {
        // `channel.thread_id` carries the Zulip topic for stream messages; DMs
        // leave it None and use `channel.channel_id` as a CSV of recipient IDs.
        let mut form: Vec<(&str, String)> = Vec::new();
        if let Some(topic) = &channel.thread_id {
            form.push(("type", "stream".to_string()));
            form.push(("to", channel.channel_id.clone()));
            form.push(("topic", topic.clone()));
        } else {
            form.push(("type", "private".to_string()));
            // For DMs, `channel_id` holds the JSON array of user IDs (Zulip accepts
            // a JSON-array literal as the `to` field).
            form.push(("to", channel.channel_id.clone()));
        }
        form.push(("content", content.to_string()));

        let resp = self
            .api_call(reqwest::Method::POST, "/api/v1/messages", Some(&form), None)
            .await?;
        let id = resp
            .get("id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow!("no id in send_message response"))?;
        Ok(MessageRef {
            channel: ChannelRef {
                platform: "zulip".into(),
                channel_id: channel.channel_id.clone(),
                thread_id: channel.thread_id.clone(),
                parent_id: None,
                origin_event_id: None,
            },
            message_id: id.to_string(),
        })
    }

    async fn create_thread(
        &self,
        channel: &ChannelRef,
        _trigger_msg: &MessageRef,
        _title: &str,
    ) -> Result<ChannelRef> {
        // Zulip has no distinct thread object — the (stream, topic) pair *is*
        // the conversation. Pin to whatever topic the trigger message used.
        Ok(ChannelRef {
            platform: "zulip".into(),
            channel_id: channel.channel_id.clone(),
            thread_id: channel.thread_id.clone(),
            parent_id: None,
            origin_event_id: None,
        })
    }

    async fn add_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()> {
        let name = unicode_to_zulip_emoji(emoji);
        let form = [("emoji_name", name.to_string())];
        let path = format!("/api/v1/messages/{}/reactions", msg.message_id);
        match self
            .api_call(reqwest::Method::POST, &path, Some(&form), None)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("REACTION_ALREADY_EXISTS") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn remove_reaction(&self, msg: &MessageRef, emoji: &str) -> Result<()> {
        let name = unicode_to_zulip_emoji(emoji);
        let query = [("emoji_name", name.to_string())];
        let path = format!("/api/v1/messages/{}/reactions", msg.message_id);
        match self
            .api_call(reqwest::Method::DELETE, &path, None, Some(&query))
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.to_string().contains("REACTION_DOES_NOT_EXIST") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn edit_message(&self, msg: &MessageRef, content: &str) -> Result<()> {
        let form = [("content", content.to_string())];
        let path = format!("/api/v1/messages/{}", msg.message_id);
        self.api_call(reqwest::Method::PATCH, &path, Some(&form), None)
            .await?;
        Ok(())
    }

    fn use_streaming(&self, other_bot_present: bool) -> bool {
        !other_bot_present
    }

    async fn start_typing(&self, channel: &ChannelRef) -> Result<()> {
        let form = typing_form("start", channel);
        self.api_call(reqwest::Method::POST, "/api/v1/typing", Some(&form), None)
            .await?;
        Ok(())
    }

    async fn stop_typing(&self, channel: &ChannelRef) -> Result<()> {
        let form = typing_form("stop", channel);
        self.api_call(reqwest::Method::POST, "/api/v1/typing", Some(&form), None)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

/// Parameters bundled for `run_zulip_adapter` so the call site in `main.rs`
/// stays readable as the adapter accrues knobs.
pub struct ZulipParams {
    pub allow_all_channels: bool,
    pub allow_all_users: bool,
    pub allowed_channels: HashSet<String>,
    pub allowed_users: HashSet<String>,
    pub allow_bot_messages: AllowBots,
    pub trusted_bot_ids: HashSet<String>,
    pub allow_user_messages: AllowUsers,
    pub max_bot_turns: u32,
    pub stt_config: SttConfig,
}

/// Outcome of classifying a Zulip `/events` error — used to keep the loop's
/// control flow readable and to make recovery branches explicit (and
/// unit-testable).
#[derive(Debug, PartialEq, Eq)]
pub enum PollOutcome {
    /// Queue is gone (BAD_EVENT_QUEUE_ID or HTTP 400). Caller must re-register.
    QueueGone,
    /// Recoverable error (5xx, transient network); caller backs off and retries.
    Transient,
}

/// Classify a Zulip `/events` error for the recovery branches above.
///
/// Detection rules per Constraint C3:
/// - `result == "error"` with `code == "BAD_EVENT_QUEUE_ID"` → `QueueGone`.
/// - HTTP 400 (any body) on `/events` → `QueueGone` (defensive fallback when
///   the server omits the code, e.g. mid-restart).
/// - Anything else → `Transient`.
pub fn classify_events_error(err: &anyhow::Error) -> PollOutcome {
    let s = err.to_string();
    if s.contains("BAD_EVENT_QUEUE_ID") || s.contains("HTTP 400") {
        PollOutcome::QueueGone
    } else {
        PollOutcome::Transient
    }
}

/// Register a new event queue. Returns `(queue_id, last_event_id)`.
async fn register_queue(adapter: &ZulipAdapter) -> Result<(String, i64)> {
    let form = [
        ("event_types", r#"["message"]"#.to_string()),
        ("apply_markdown", "false".to_string()),
    ];
    let resp = adapter
        .api_call(reqwest::Method::POST, "/api/v1/register", Some(&form), None)
        .await?;
    let queue_id = resp
        .get("queue_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("no queue_id in register response"))?
        .to_string();
    let last_event_id = resp
        .get("last_event_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    Ok((queue_id, last_event_id))
}

/// Long-poll `/events`. Returns `Ok(events_array)` on success, `Err` on
/// transport/server failure (caller uses `classify_events_error` to recover).
async fn poll_events(
    adapter: &ZulipAdapter,
    queue_id: &str,
    last_event_id: i64,
) -> Result<serde_json::Value> {
    let query = [
        ("queue_id", queue_id.to_string()),
        ("last_event_id", last_event_id.to_string()),
    ];
    let resp = adapter
        .api_call(reqwest::Method::GET, "/api/v1/events", None, Some(&query))
        .await?;
    Ok(resp
        .get("events")
        .cloned()
        .unwrap_or(serde_json::Value::Array(vec![])))
}

/// Bundle of fields the event loop hands to a sink per accepted message.
/// Kept as a struct so adding new fields (e.g. attachment URLs) does not break
/// existing implementations.
#[derive(Debug, Clone)]
pub struct ZulipDispatchedMessage {
    /// ACP session/thread key as produced by `thread_key_for_event`.
    pub thread_key: String,
    /// Stream ID as numeric string for stream messages, `None` for DMs.
    pub stream_id: Option<String>,
    /// Stream topic — empty for DMs.
    pub topic: String,
    /// Sender's Zulip user ID as numeric string.
    pub sender_id: String,
    /// Zulip integer message ID as string (for reactions / streaming edits).
    pub message_id: String,
    /// Verbatim message body.
    pub content: String,
}

/// Bundle of fields the event loop hands to the sink when an `/eom` command
/// is recognized.
#[derive(Debug, Clone)]
pub struct EomCommand {
    pub thread_key: String,
    pub stream_id: Option<String>,
    pub topic: String,
    pub sender_id: String,
    pub message_id: String,
    /// Trimmed args following `/eom`; empty string means no-arg form.
    pub args: String,
}

/// Trait surface for the dispatch side-effect: the event loop calls this for
/// every accepted message. Production wires this to the broker's `Dispatcher`;
/// unit tests use a recording double to assert thread-key correctness.
#[async_trait]
pub trait ZulipMessageSink: Send + Sync {
    async fn dispatch(&self, evt: ZulipDispatchedMessage);
    async fn handle_eom(&self, cmd: EomCommand);
}

/// Production sink: builds the broker `SenderContext` + `BufferedMessage` and
/// submits to the shared `Dispatcher`. Built once at startup and shared across
/// the event loop via `Arc`.
pub struct BrokerSink {
    adapter: Arc<ZulipAdapter>,
    dispatcher: Arc<crate::dispatch::Dispatcher>,
    pool: Arc<crate::acp::SessionPool>,
}

impl BrokerSink {
    pub fn new(
        adapter: Arc<ZulipAdapter>,
        dispatcher: Arc<crate::dispatch::Dispatcher>,
        pool: Arc<crate::acp::SessionPool>,
    ) -> Self {
        Self {
            adapter,
            dispatcher,
            pool,
        }
    }
}

#[async_trait]
impl ZulipMessageSink for BrokerSink {
    async fn dispatch(&self, evt: ZulipDispatchedMessage) {
        self.dispatch_impl(evt).await
    }

    async fn handle_eom(&self, cmd: EomCommand) {
        if let Err(e) = self.pool.reset_session(&cmd.thread_key).await {
            warn!(error = %e, thread_key = %cmd.thread_key, "zulip /eom: pool.reset_session failed");
        }
        // thread_key is `zulip:<thread_id>` — strip the `zulip:` prefix to get
        // the dispatcher's thread_id (mirrors gateway.rs:798-800).
        let thread_id_for_dispatcher = cmd
            .thread_key
            .strip_prefix("zulip:")
            .unwrap_or(&cmd.thread_key);
        self.dispatcher
            .cancel_buffered_thread("zulip", thread_id_for_dispatcher);

        let channel_id = cmd.stream_id.clone().unwrap_or_default();
        let thread_id_opt = if cmd.topic.is_empty() {
            None
        } else {
            Some(cmd.topic.clone())
        };
        let trigger_channel = ChannelRef {
            platform: "zulip".into(),
            channel_id: channel_id.clone(),
            thread_id: thread_id_opt.clone(),
            parent_id: None,
            origin_event_id: None,
        };

        let has_args = !cmd.args.trim().is_empty();
        let ack_text = if has_args {
            "🛑 Eyes-on-me — aborted current task, picking up new instruction…"
        } else {
            "🛑 Eyes-on-me — aborted current task."
        };
        if let Err(e) = self.adapter.send_message(&trigger_channel, ack_text).await {
            warn!(error = %e, "zulip /eom: ack send_message failed");
        }
        if !has_args {
            return;
        }

        let trigger_msg = MessageRef {
            channel: trigger_channel.clone(),
            message_id: cmd.message_id.clone(),
        };
        let sender = crate::adapter::SenderContext {
            schema: "openab.sender.v1".into(),
            sender_id: cmd.sender_id.clone(),
            sender_name: cmd.sender_id.clone(),
            display_name: cmd.sender_id.clone(),
            channel: "zulip".into(),
            channel_id: channel_id.clone(),
            thread_id: thread_id_opt.clone(),
            is_bot: false,
            timestamp: None,
            message_id: Some(cmd.message_id.clone()),
            receiver_id: None,
        };
        let sender_json = serde_json::to_string(&sender).unwrap_or_else(|_| "{}".into());
        let estimated_tokens = crate::dispatch::estimate_tokens(&cmd.args, &[]);
        let adapter_dyn: Arc<dyn ChatAdapter> = self.adapter.clone();
        let buf = crate::dispatch::BufferedMessage {
            sender_json,
            sender_name: cmd.sender_id.clone(),
            prompt: cmd.args,
            extra_blocks: Vec::new(),
            trigger_msg,
            arrived_at: std::time::Instant::now(),
            estimated_tokens,
            other_bot_present: false,
        };
        if let Err(e) = self
            .dispatcher
            .submit(cmd.thread_key, trigger_channel, adapter_dyn, buf)
            .await
        {
            warn!(error = %e, "zulip /eom: dispatcher submit failed");
        }
    }
}

impl BrokerSink {
    async fn dispatch_impl(&self, evt: ZulipDispatchedMessage) {
        // Build the trigger MessageRef so reactions / streaming edits land on
        // the originating Zulip message.
        let channel_id = evt.stream_id.clone().unwrap_or_default();
        let thread_id_opt = if evt.topic.is_empty() {
            None
        } else {
            Some(evt.topic.clone())
        };
        let trigger_channel = ChannelRef {
            platform: "zulip".into(),
            channel_id: channel_id.clone(),
            thread_id: thread_id_opt.clone(),
            parent_id: None,
            origin_event_id: None,
        };
        let trigger_msg = MessageRef {
            channel: trigger_channel.clone(),
            message_id: evt.message_id.clone(),
        };

        // v1: minimal SenderContext. User-display-name resolution is deferred
        // (operator sees sender_id verbatim). Logged as a known limitation.
        let sender = crate::adapter::SenderContext {
            schema: "openab.sender.v1".into(),
            sender_id: evt.sender_id.clone(),
            sender_name: evt.sender_id.clone(),
            display_name: evt.sender_id.clone(),
            channel: "zulip".into(),
            channel_id: channel_id.clone(),
            thread_id: thread_id_opt.clone(),
            is_bot: false,
            timestamp: None,
            message_id: Some(evt.message_id.clone()),
            receiver_id: None,
        };
        let sender_json = serde_json::to_string(&sender).unwrap_or_else(|_| "{}".into());
        let estimated_tokens = crate::dispatch::estimate_tokens(&evt.content, &[]);
        let adapter_dyn: Arc<dyn ChatAdapter> = self.adapter.clone();
        let buf = crate::dispatch::BufferedMessage {
            sender_json,
            sender_name: evt.sender_id.clone(),
            prompt: evt.content.clone(),
            extra_blocks: Vec::new(),
            trigger_msg,
            arrived_at: std::time::Instant::now(),
            estimated_tokens,
            other_bot_present: false,
        };
        if let Err(e) = self
            .dispatcher
            .submit(evt.thread_key, trigger_channel, adapter_dyn, buf)
            .await
        {
            warn!(error = %e, "zulip dispatcher submit failed");
        }
    }
}

/// Run the Zulip event loop. Long-polls until shutdown.
///
/// Recovery semantics:
/// - `BAD_EVENT_QUEUE_ID` or HTTP 400 on `/events` → re-register, resume.
/// - 5xx / transport error → exponential back-off (1s, 2s, 5s, 10s capped).
/// - Shutdown signal → return `Ok(())` within ~5s.
pub async fn run_zulip_adapter(
    adapter: Arc<ZulipAdapter>,
    params: ZulipParams,
    sink: Arc<dyn ZulipMessageSink>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    info!("starting zulip adapter");
    // Unused-for-now params are documented; silence dead-code at compile.
    let _ = (
        &params.allow_bot_messages,
        &params.trusted_bot_ids,
        &params.allow_user_messages,
        params.max_bot_turns,
        &params.stt_config,
    );

    let mut backoff_idx: usize = 0;
    const BACKOFFS: &[u64] = &[1, 2, 5, 10];

    'outer: loop {
        if *shutdown_rx.borrow() {
            info!("zulip adapter shutting down");
            return Ok(());
        }

        // Register / re-register a queue.
        let (queue_id, mut last_event_id) = match register_queue(&adapter).await {
            Ok(v) => {
                info!(queue_id = %v.0, "zulip event queue registered");
                backoff_idx = 0;
                v
            }
            Err(e) => {
                let secs = BACKOFFS[backoff_idx.min(BACKOFFS.len() - 1)];
                warn!(error = %e, secs, "zulip /register failed, backing off");
                backoff_idx = (backoff_idx + 1).min(BACKOFFS.len() - 1);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => continue 'outer,
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() { return Ok(()); }
                        continue 'outer;
                    }
                }
            }
        };

        loop {
            // Cooperative shutdown check.
            if *shutdown_rx.borrow() {
                info!("zulip adapter shutting down");
                return Ok(());
            }

            let poll_fut = poll_events(&adapter, &queue_id, last_event_id);
            let events_result = tokio::select! {
                r = poll_fut => r,
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("zulip adapter shutting down");
                        return Ok(());
                    }
                    continue;
                }
            };

            let events = match events_result {
                Ok(ev) => ev,
                Err(e) => match classify_events_error(&e) {
                    PollOutcome::QueueGone => {
                        info!(error = %e, "zulip queue expired, re-registering");
                        continue 'outer;
                    }
                    PollOutcome::Transient => {
                        let secs = BACKOFFS[backoff_idx.min(BACKOFFS.len() - 1)];
                        warn!(error = %e, secs, "zulip /events transient error, backing off");
                        backoff_idx = (backoff_idx + 1).min(BACKOFFS.len() - 1);
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {},
                            _ = shutdown_rx.changed() => {
                                if *shutdown_rx.borrow() { return Ok(()); }
                            }
                        }
                        continue;
                    }
                },
            };
            backoff_idx = 0;

            let arr = events.as_array().cloned().unwrap_or_default();
            for ev in arr {
                if let Some(id) = ev.get("id").and_then(|v| v.as_i64()) {
                    if id > last_event_id {
                        last_event_id = id;
                    }
                }
                if ev.get("type").and_then(|v| v.as_str()) != Some("message") {
                    continue;
                }
                // Zulip can deliver the message either directly on the event
                // (`stream_id`/`subject` at top level) or nested under `message`.
                let body = ev.get("message").unwrap_or(&ev);
                let stream_id = body
                    .get("stream_id")
                    .and_then(|v| v.as_i64())
                    .map(|i| i.to_string());
                let topic = body
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let sender_id = match body.get("sender_id").and_then(|v| v.as_i64()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                let content = body
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let message_id = body
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .map(|i| i.to_string())
                    .unwrap_or_default();

                if !allowlist_accepts(
                    stream_id.as_deref(),
                    &sender_id,
                    params.allow_all_channels,
                    params.allow_all_users,
                    &params.allowed_channels,
                    &params.allowed_users,
                ) {
                    debug!(stream_id = ?stream_id, sender_id = %sender_id, "zulip allowlist denied");
                    continue;
                }

                let key = if let Some(sid_str) = &stream_id {
                    let sid: u64 = sid_str.parse().unwrap_or(0);
                    thread_key_for_event(&ZulipEventKind::Stream {
                        stream_id: sid,
                        topic: &topic,
                    })
                } else {
                    // DM: collect `display_recipient` user IDs.
                    let ids: Vec<u64> = body
                        .get("display_recipient")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|r| {
                                    r.get("id").and_then(|v| v.as_i64()).map(|i| i as u64)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    thread_key_for_event(&ZulipEventKind::Dm { user_ids: &ids })
                };
                debug!(thread_key = %key, "zulip message dispatched");
                if let Some(args) = parse_eom(&content) {
                    sink.handle_eom(EomCommand {
                        thread_key: key,
                        stream_id,
                        topic,
                        sender_id,
                        message_id,
                        args,
                    })
                    .await;
                    continue;
                }
                sink.dispatch(ZulipDispatchedMessage {
                    thread_key: key,
                    stream_id,
                    topic,
                    sender_id,
                    message_id,
                    content,
                })
                .await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // --- ZulipThreadKey ---

    #[test]
    fn thread_key_stream_lowercases_and_keeps_internal_spaces() {
        let kind = ZulipEventKind::Stream {
            stream_id: 42,
            topic: "Deploy 2026-Q2",
        };
        assert_eq!(
            thread_key_for_event(&kind),
            "zulip:stream:42:deploy 2026-q2"
        );
    }

    #[test]
    fn thread_key_stream_trims_surrounding_whitespace() {
        let kind = ZulipEventKind::Stream {
            stream_id: 42,
            topic: "  Deploy 2026-Q2  ",
        };
        assert_eq!(
            thread_key_for_event(&kind),
            "zulip:stream:42:deploy 2026-q2"
        );
    }

    #[test]
    fn thread_key_dm_sorts_ascending() {
        let ids = [7u64, 3, 11];
        let kind = ZulipEventKind::Dm { user_ids: &ids };
        assert_eq!(thread_key_for_event(&kind), "zulip:dm:3,7,11");
    }

    #[test]
    fn thread_key_dm_is_order_independent() {
        let ids_a = [11u64, 7, 3];
        let ids_b = [3u64, 7, 11];
        assert_eq!(
            thread_key_for_event(&ZulipEventKind::Dm { user_ids: &ids_a }),
            thread_key_for_event(&ZulipEventKind::Dm { user_ids: &ids_b }),
        );
        assert_eq!(
            thread_key_for_event(&ZulipEventKind::Dm { user_ids: &ids_a }),
            "zulip:dm:3,7,11"
        );
    }

    // --- ZulipStreamingPolicy ---

    #[test]
    fn streaming_on_when_no_other_bot() {
        let a = ZulipAdapter::new("http://x", "b@x", "k");
        assert!(a.use_streaming(false));
    }

    #[test]
    fn streaming_off_when_other_bot_present() {
        let a = ZulipAdapter::new("http://x", "b@x", "k");
        assert!(!a.use_streaming(true));
    }

    // --- ZulipCreateThread ---

    #[tokio::test]
    async fn create_thread_echoes_stream_and_topic() {
        let a = ZulipAdapter::new("http://x", "b@x", "k");
        let channel = ChannelRef {
            platform: "zulip".into(),
            channel_id: "42".into(),
            thread_id: Some("deploy".into()),
            parent_id: None,
            origin_event_id: None,
        };
        let trigger = MessageRef {
            channel: channel.clone(),
            message_id: "9001".into(),
        };
        let out = a
            .create_thread(&channel, &trigger, "anything")
            .await
            .unwrap();
        assert_eq!(out.platform, "zulip");
        assert_eq!(out.channel_id, "42");
        assert_eq!(out.thread_id.as_deref(), Some("deploy"));
    }

    // --- ZulipDispatchAllowlistGate ---

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn allowlist_accepts_when_both_lists_match() {
        assert!(allowlist_accepts(
            Some("42"),
            "7",
            false,
            false,
            &set(&["42"]),
            &set(&["7"]),
        ));
    }

    #[test]
    fn allowlist_rejects_unknown_user() {
        assert!(!allowlist_accepts(
            Some("42"),
            "999",
            false,
            false,
            &set(&["42"]),
            &set(&["7"]),
        ));
    }

    #[test]
    fn allowlist_rejects_unknown_channel() {
        assert!(!allowlist_accepts(
            Some("99"),
            "7",
            false,
            false,
            &set(&["42"]),
            &set(&["7"]),
        ));
    }

    #[test]
    fn allowlist_allow_all_overrides_lists() {
        assert!(allowlist_accepts(
            Some("99"),
            "999",
            true,
            true,
            &set(&["42"]),
            &set(&["7"]),
        ));
    }

    // --- EomCommandParser ---

    #[test]
    fn parse_eom_with_args() {
        assert_eq!(
            parse_eom("/eom hello world"),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn parse_eom_preserves_interior_whitespace() {
        assert_eq!(parse_eom("/eom  do  this  "), Some("do  this".to_string()));
    }

    #[test]
    fn parse_eom_no_args() {
        assert_eq!(parse_eom("/eom"), Some(String::new()));
    }

    #[test]
    fn parse_eom_whitespace_only_args() {
        assert_eq!(parse_eom("/eom   "), Some(String::new()));
    }

    #[test]
    fn parse_eom_not_at_start_is_rejected() {
        assert_eq!(parse_eom("Hey /eom inside"), None);
    }

    #[test]
    fn parse_eom_rejects_prefix_extension() {
        assert_eq!(parse_eom("/eomx blah"), None);
    }

    #[test]
    fn parse_eom_empty_body_is_none() {
        assert_eq!(parse_eom(""), None);
    }

    #[test]
    fn parse_eom_leading_whitespace_disqualifies() {
        assert_eq!(parse_eom("  /eom hi"), None);
    }

    // --- HTTP test plumbing -------------------------------------------------

    /// One canned HTTP response.
    struct Canned {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: String,
    }

    /// Spin up a tiny TCP server that serves a queue of canned responses, one
    /// per incoming connection. Returns the bound base URL (without trailing
    /// slash) so the adapter can be pointed at it.
    async fn spawn_mock(canned: Vec<Canned>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        tokio::spawn(async move {
            for c in canned {
                let (mut sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                // Drain the request (best-effort, until \r\n\r\n).
                let mut buf = [0u8; 4096];
                let mut total = String::new();
                while let Ok(n) = sock.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    total.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if total.contains("\r\n\r\n") {
                        // For requests with a body, the Content-Length header tells
                        // us if there is more — but for these tests we don't need
                        // to parse the body, just respond.
                        break;
                    }
                }
                let mut resp = format!("HTTP/1.1 {} OK\r\n", c.status);
                for (k, v) in &c.headers {
                    resp.push_str(&format!("{k}: {v}\r\n"));
                }
                resp.push_str(&format!("Content-Length: {}\r\n", c.body.len()));
                resp.push_str("Connection: close\r\n\r\n");
                resp.push_str(&c.body);
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}")
    }

    // --- ZulipApiClient ---

    #[tokio::test]
    async fn api_call_retries_once_on_429_with_retry_after() {
        let canned = vec![
            Canned {
                status: 429,
                headers: vec![("Retry-After", "1".into())],
                body: r#"{"result":"error","code":"RATE_LIMIT","msg":"slow"}"#.into(),
            },
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","id":9001}"#.into(),
            },
        ];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let start = std::time::Instant::now();
        let resp = adapter
            .api_call(reqwest::Method::POST, "/api/v1/messages", Some(&[]), None)
            .await
            .expect("should succeed after retry");
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() >= 900,
            "should have slept ~1s, got {elapsed:?}"
        );
        assert_eq!(resp["id"].as_i64(), Some(9001));
    }

    #[tokio::test]
    async fn api_call_surfaces_zulip_error_envelope() {
        let canned = vec![Canned {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"error","code":"BAD_REQUEST","msg":"bad"}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let err = adapter
            .api_call(reqwest::Method::POST, "/api/v1/messages", Some(&[]), None)
            .await
            .unwrap_err();
        let s = err.to_string();
        assert!(s.contains("BAD_REQUEST"), "missing code: {s}");
        assert!(s.contains("bad"), "missing msg: {s}");
    }

    #[tokio::test]
    async fn api_call_401_returns_err_no_retry() {
        let canned = vec![Canned {
            status: 401,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"error","msg":"unauth"}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let err = adapter
            .api_call(reqwest::Method::GET, "/api/v1/users/me", None, None)
            .await
            .unwrap_err();
        let s = err.to_string();
        assert!(s.contains("401"), "missing status: {s}");
    }

    // --- ZulipSendMessage ---

    #[tokio::test]
    async fn send_message_returns_message_ref_with_integer_id_as_string() {
        let canned = vec![Canned {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"success","id":9001}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let channel = ChannelRef {
            platform: "zulip".into(),
            channel_id: "42".into(),
            thread_id: Some("deploy".into()),
            parent_id: None,
            origin_event_id: None,
        };
        let m = adapter.send_message(&channel, "hello").await.unwrap();
        assert_eq!(m.message_id, "9001");
        assert_eq!(m.channel.platform, "zulip");
        assert_eq!(m.channel.channel_id, "42");
        assert_eq!(m.channel.thread_id.as_deref(), Some("deploy"));
    }

    #[tokio::test]
    async fn send_message_surfaces_stream_does_not_exist() {
        let canned = vec![Canned {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"error","code":"STREAM_DOES_NOT_EXIST","msg":"no stream"}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let channel = ChannelRef {
            platform: "zulip".into(),
            channel_id: "42".into(),
            thread_id: Some("deploy".into()),
            parent_id: None,
            origin_event_id: None,
        };
        let err = adapter.send_message(&channel, "hello").await.unwrap_err();
        assert!(err.to_string().contains("STREAM_DOES_NOT_EXIST"));
    }

    // --- ZulipEditMessage ---

    #[tokio::test]
    async fn edit_message_succeeds_on_200() {
        let canned = vec![Canned {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"success"}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let msg = MessageRef {
            channel: ChannelRef {
                platform: "zulip".into(),
                channel_id: "42".into(),
                thread_id: Some("deploy".into()),
                parent_id: None,
                origin_event_id: None,
            },
            message_id: "9001".into(),
        };
        adapter.edit_message(&msg, "hello world").await.unwrap();
    }

    #[tokio::test]
    async fn edit_message_surfaces_history_disabled() {
        let canned = vec![Canned {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"error","code":"MESSAGE_EDIT_HISTORY_DISABLED","msg":"no edits"}"#
                .into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let msg = MessageRef {
            channel: ChannelRef {
                platform: "zulip".into(),
                channel_id: "42".into(),
                thread_id: Some("deploy".into()),
                parent_id: None,
                origin_event_id: None,
            },
            message_id: "9001".into(),
        };
        let err = adapter.edit_message(&msg, "hello world").await.unwrap_err();
        assert!(err.to_string().contains("MESSAGE_EDIT_HISTORY_DISABLED"));
    }

    // --- ZulipReactionToggle ---

    #[tokio::test]
    async fn add_reaction_succeeds_on_200() {
        let canned = vec![Canned {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"success"}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let msg = MessageRef {
            channel: ChannelRef {
                platform: "zulip".into(),
                channel_id: "42".into(),
                thread_id: Some("deploy".into()),
                parent_id: None,
                origin_event_id: None,
            },
            message_id: "9001".into(),
        };
        adapter.add_reaction(&msg, "👀").await.unwrap();
    }

    #[tokio::test]
    async fn add_reaction_swallows_already_exists() {
        let canned = vec![Canned {
            status: 400,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"error","code":"REACTION_ALREADY_EXISTS","msg":"dup"}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let msg = MessageRef {
            channel: ChannelRef {
                platform: "zulip".into(),
                channel_id: "42".into(),
                thread_id: Some("deploy".into()),
                parent_id: None,
                origin_event_id: None,
            },
            message_id: "9001".into(),
        };
        // Even though HTTP 400, the error string includes the code → swallowed.
        adapter.add_reaction(&msg, "👀").await.unwrap();
    }

    #[tokio::test]
    async fn add_reaction_propagates_unrelated_400() {
        let canned = vec![Canned {
            status: 400,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"error","code":"BAD_EMOJI_NAME","msg":"nope"}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let msg = MessageRef {
            channel: ChannelRef {
                platform: "zulip".into(),
                channel_id: "42".into(),
                thread_id: Some("deploy".into()),
                parent_id: None,
                origin_event_id: None,
            },
            message_id: "9001".into(),
        };
        let err = adapter.add_reaction(&msg, "👀").await.unwrap_err();
        assert!(err.to_string().contains("BAD_EMOJI_NAME"));
    }

    // --- ZulipEventLoop ---

    /// One row recorded by the test sink. Kept as a named struct so clippy
    /// doesn't flag the otherwise-fine `Vec<(_,_,_,_)>` as overly complex.
    #[derive(Debug)]
    struct DispatchedEvent {
        thread_key: String,
        stream_id: Option<String>,
        sender_id: String,
        #[allow(dead_code)]
        content: String,
    }

    /// Recording sink — records every dispatched event for assertions.
    struct RecordingSink {
        log: Mutex<Vec<DispatchedEvent>>,
        eom_log: Mutex<Vec<EomCommand>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                log: Mutex::new(Vec::new()),
                eom_log: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ZulipMessageSink for RecordingSink {
        async fn dispatch(&self, evt: ZulipDispatchedMessage) {
            self.log.lock().unwrap().push(DispatchedEvent {
                thread_key: evt.thread_key,
                stream_id: evt.stream_id,
                sender_id: evt.sender_id,
                content: evt.content,
            });
        }
        async fn handle_eom(&self, cmd: EomCommand) {
            self.eom_log.lock().unwrap().push(cmd);
        }
    }

    #[test]
    fn classify_events_error_detects_bad_queue_id() {
        let e = anyhow!("Zulip /events: BAD_EVENT_QUEUE_ID: queue gone");
        assert_eq!(classify_events_error(&e), PollOutcome::QueueGone);
    }

    #[test]
    fn classify_events_error_detects_http_400_fallback() {
        let e = anyhow!("Zulip /events: HTTP 400 ");
        assert_eq!(classify_events_error(&e), PollOutcome::QueueGone);
    }

    #[test]
    fn classify_events_error_treats_5xx_as_transient() {
        let e = anyhow!("Zulip /events: HTTP 500 ");
        assert_eq!(classify_events_error(&e), PollOutcome::Transient);
    }

    fn make_params(channels: &[&str]) -> ZulipParams {
        ZulipParams {
            allow_all_channels: channels.is_empty(),
            allow_all_users: true,
            allowed_channels: set(channels),
            allowed_users: HashSet::new(),
            allow_bot_messages: AllowBots::Off,
            trusted_bot_ids: HashSet::new(),
            allow_user_messages: AllowUsers::Involved,
            max_bot_turns: 100,
            stt_config: SttConfig::default(),
        }
    }

    /// Drive the event loop with: register → events → shutdown.
    /// Verifies allowlist-accepted events reach the sink with the expected key.
    #[tokio::test(flavor = "current_thread")]
    async fn event_loop_dispatches_allowed_stream_event() {
        let canned = vec![
            // /register response
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","queue_id":"q1","last_event_id":-1}"#.into(),
            },
            // /events first poll — one message event
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","events":[{"id":1,"type":"message","message":{"stream_id":42,"subject":"x","sender_id":7,"content":"hi"}}]}"#.into(),
            },
            // /events second poll — empty (loop will wait for shutdown)
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","events":[]}"#.into(),
            },
        ];
        let base = spawn_mock(canned).await;
        let adapter = Arc::new(ZulipAdapter::new(base, "b@x", "k"));
        let params = make_params(&["42"]);
        let sink = Arc::new(RecordingSink::new());
        let (tx, rx) = watch::channel(false);

        let sink_clone = sink.clone();
        let handle =
            tokio::spawn(async move { run_zulip_adapter(adapter, params, sink_clone, rx).await });

        // Allow the loop to register + poll once + dispatch.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = tx.send(true);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("loop should exit within 5s")
            .expect("task should join");
        res.expect("loop should return Ok");

        let log = sink.log.lock().unwrap();
        assert_eq!(
            log.len(),
            1,
            "expected 1 dispatched message, got {}",
            log.len()
        );
        assert_eq!(log[0].thread_key, "zulip:stream:42:x");
        assert_eq!(log[0].stream_id.as_deref(), Some("42"));
        assert_eq!(log[0].sender_id, "7");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn event_loop_drops_event_outside_allowlist() {
        let canned = vec![
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","queue_id":"q1","last_event_id":-1}"#.into(),
            },
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","events":[{"id":1,"type":"message","message":{"stream_id":42,"subject":"x","sender_id":7,"content":"hi"}}]}"#.into(),
            },
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","events":[]}"#.into(),
            },
        ];
        let base = spawn_mock(canned).await;
        let adapter = Arc::new(ZulipAdapter::new(base, "b@x", "k"));
        // allowed_channels = ["99"] → 42 should be denied
        let params = make_params(&["99"]);
        let sink = Arc::new(RecordingSink::new());
        let (tx, rx) = watch::channel(false);

        let sink_clone = sink.clone();
        let handle =
            tokio::spawn(async move { run_zulip_adapter(adapter, params, sink_clone, rx).await });

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = tx.send(true);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;

        let log = sink.log.lock().unwrap();
        assert!(
            log.is_empty(),
            "expected no dispatched message, got {log:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn event_loop_reregisters_on_bad_event_queue_id() {
        let canned = vec![
            // first /register
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","queue_id":"q1","last_event_id":-1}"#.into(),
            },
            // /events → BAD_EVENT_QUEUE_ID (200 envelope-error)
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"error","code":"BAD_EVENT_QUEUE_ID","msg":"queue gone"}"#.into(),
            },
            // second /register
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","queue_id":"q2","last_event_id":-1}"#.into(),
            },
            // /events on new queue: one allowed message
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","events":[{"id":1,"type":"message","message":{"stream_id":42,"subject":"y","sender_id":7,"content":"hi"}}]}"#.into(),
            },
            // empty poll to park the loop
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","events":[]}"#.into(),
            },
        ];
        let base = spawn_mock(canned).await;
        let adapter = Arc::new(ZulipAdapter::new(base, "b@x", "k"));
        let params = make_params(&["42"]);
        let sink = Arc::new(RecordingSink::new());
        let (tx, rx) = watch::channel(false);

        let sink_clone = sink.clone();
        let handle =
            tokio::spawn(async move { run_zulip_adapter(adapter, params, sink_clone, rx).await });

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let _ = tx.send(true);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(
            res.is_ok(),
            "loop should exit within 5s after BAD_EVENT_QUEUE_ID recovery"
        );

        let log = sink.log.lock().unwrap();
        assert_eq!(
            log.len(),
            1,
            "expected message after re-register, got {log:?}"
        );
        assert_eq!(log[0].thread_key, "zulip:stream:42:y");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn event_loop_reregisters_on_http_400_fallback() {
        let canned = vec![
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","queue_id":"q1","last_event_id":-1}"#.into(),
            },
            // /events returns HTTP 400 with no code → fallback detection should trigger.
            Canned {
                status: 400,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{}"#.into(),
            },
            // /register (recovery)
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","queue_id":"q2","last_event_id":-1}"#.into(),
            },
            // empty poll
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","events":[]}"#.into(),
            },
        ];
        let base = spawn_mock(canned).await;
        let adapter = Arc::new(ZulipAdapter::new(base, "b@x", "k"));
        let params = make_params(&[]); // allow_all_channels via empty list
        let sink = Arc::new(RecordingSink::new());
        let (tx, rx) = watch::channel(false);

        let sink_clone = sink.clone();
        let handle =
            tokio::spawn(async move { run_zulip_adapter(adapter, params, sink_clone, rx).await });

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let _ = tx.send(true);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(
            res.is_ok(),
            "loop should exit within 5s after HTTP 400 recovery"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn event_loop_returns_ok_on_shutdown_during_long_poll() {
        let canned = vec![
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","queue_id":"q1","last_event_id":-1}"#.into(),
            },
            // No second response → the loop will be parked in poll_events.
        ];
        let base = spawn_mock(canned).await;
        let adapter = Arc::new(ZulipAdapter::new(base, "b@x", "k"));
        let params = make_params(&[]);
        let sink = Arc::new(RecordingSink::new());
        let (tx, rx) = watch::channel(false);

        let sink_clone = sink.clone();
        let handle =
            tokio::spawn(async move { run_zulip_adapter(adapter, params, sink_clone, rx).await });

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let _ = tx.send(true);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("loop should exit within 5s of shutdown signal");
        res.expect("task join").expect("loop Ok");
    }

    // --- BrokerSinkHandleEom -----------------------------------------------

    use crate::dispatch::{
        BatchGrouping, DispatchTarget, Dispatcher, DEFAULT_CONSUMER_IDLE_TIMEOUT,
    };
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// Test DispatchTarget that just counts how many times `ensure_session` is
    /// invoked. A real submit's consumer task calls `ensure_session` once before
    /// touching ACP, so a non-zero count proves the submit reached the consumer.
    struct CountingTarget {
        ensure_calls: Arc<AtomicUsize>,
        reactions: crate::config::ReactionsConfig,
    }

    #[async_trait]
    impl DispatchTarget for CountingTarget {
        fn reactions_config(&self) -> &crate::config::ReactionsConfig {
            &self.reactions
        }
        async fn ensure_session(&self, _session_key: &str) -> Result<()> {
            self.ensure_calls.fetch_add(1, AtomicOrdering::SeqCst);
            // Return Err so the consumer aborts the turn quickly without
            // attempting to stream against a non-existent ACP process.
            Err(anyhow!("test target: no ACP backing"))
        }
        async fn stream_prompt_blocks(
            &self,
            _adapter: &Arc<dyn ChatAdapter>,
            _session_key: &str,
            _content_blocks: Vec<crate::acp::ContentBlock>,
            _thread_channel: &ChannelRef,
            _reactions: Arc<crate::reactions::StatusReactionController>,
            _other_bot_present: bool,
        ) -> Result<()> {
            Ok(())
        }
    }

    fn make_test_dispatcher(ensure_calls: Arc<AtomicUsize>) -> Arc<Dispatcher> {
        let target = Arc::new(CountingTarget {
            ensure_calls,
            reactions: crate::config::ReactionsConfig::default(),
        });
        Arc::new(Dispatcher::with_idle_timeout(
            target,
            10,
            24_000,
            BatchGrouping::Thread,
            DEFAULT_CONSUMER_IDLE_TIMEOUT,
        ))
    }

    fn make_test_pool() -> Arc<crate::acp::SessionPool> {
        let agent_cfg = crate::config::AgentConfig {
            command: "/bin/true".into(),
            args: vec![],
            working_dir: "/tmp".into(),
            env: std::collections::HashMap::new(),
            inherit_env: vec![],
        };
        Arc::new(crate::acp::SessionPool::new(agent_cfg, 1))
    }

    #[tokio::test]
    async fn handle_eom_empty_args_acks_and_does_not_submit() {
        // One canned response: success for the ack POST /api/v1/messages.
        let canned = vec![Canned {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"success","id":4242}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = Arc::new(ZulipAdapter::new(base, "b@x", "k"));
        let ensure_calls = Arc::new(AtomicUsize::new(0));
        let dispatcher = make_test_dispatcher(ensure_calls.clone());
        let pool = make_test_pool();
        let sink = BrokerSink::new(adapter, dispatcher, pool);

        sink.handle_eom(EomCommand {
            thread_key: "zulip:stream:42:deploy".into(),
            stream_id: Some("42".into()),
            topic: "deploy".into(),
            sender_id: "7".into(),
            message_id: "9001".into(),
            args: String::new(),
        })
        .await;

        // Give any (unexpected) consumer task a chance to run.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            ensure_calls.load(AtomicOrdering::SeqCst),
            0,
            "no-arg /eom must not re-submit"
        );
    }

    #[tokio::test]
    async fn handle_eom_with_args_acks_and_submits() {
        let canned = vec![Canned {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"success","id":4242}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = Arc::new(ZulipAdapter::new(base, "b@x", "k"));
        let ensure_calls = Arc::new(AtomicUsize::new(0));
        let dispatcher = make_test_dispatcher(ensure_calls.clone());
        let pool = make_test_pool();
        let sink = BrokerSink::new(adapter, dispatcher, pool);

        sink.handle_eom(EomCommand {
            thread_key: "zulip:stream:42:deploy".into(),
            stream_id: Some("42".into()),
            topic: "deploy".into(),
            sender_id: "7".into(),
            message_id: "9001".into(),
            args: "do the thing".into(),
        })
        .await;

        // Wait briefly for the consumer task spawned by submit() to call ensure_session.
        for _ in 0..50 {
            if ensure_calls.load(AtomicOrdering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            ensure_calls.load(AtomicOrdering::SeqCst) > 0,
            "/eom with args must re-submit (consumer should reach ensure_session)"
        );
    }

    #[tokio::test]
    async fn handle_eom_continues_when_pool_reset_errors() {
        // Pool has no session for this key — reset_session returns Err, but the
        // ack and submit must still happen.
        let canned = vec![Canned {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"success","id":4242}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = Arc::new(ZulipAdapter::new(base, "b@x", "k"));
        let ensure_calls = Arc::new(AtomicUsize::new(0));
        let dispatcher = make_test_dispatcher(ensure_calls.clone());
        let pool = make_test_pool(); // empty pool → reset_session errors
        let sink = BrokerSink::new(adapter, dispatcher, pool);

        sink.handle_eom(EomCommand {
            thread_key: "zulip:stream:99:nope".into(),
            stream_id: Some("99".into()),
            topic: "nope".into(),
            sender_id: "7".into(),
            message_id: "9001".into(),
            args: "retry please".into(),
        })
        .await;

        for _ in 0..50 {
            if ensure_calls.load(AtomicOrdering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            ensure_calls.load(AtomicOrdering::SeqCst) > 0,
            "ack and submit must still happen when pool.reset_session errors"
        );
    }

    // --- EomWiredInEventLoop -----------------------------------------------

    #[tokio::test]
    async fn parse_then_route_branches_correctly() {
        // Smaller unit: verify the parse-then-route decision in isolation —
        // /eom content routes to handle_eom, plain content routes to dispatch.
        let sink = Arc::new(RecordingSink::new());

        let content_a = "hello world".to_string();
        if let Some(args) = parse_eom(&content_a) {
            sink.handle_eom(EomCommand {
                thread_key: "k".into(),
                stream_id: None,
                topic: String::new(),
                sender_id: "7".into(),
                message_id: "1".into(),
                args,
            })
            .await;
        } else {
            sink.dispatch(ZulipDispatchedMessage {
                thread_key: "k".into(),
                stream_id: None,
                topic: String::new(),
                sender_id: "7".into(),
                message_id: "1".into(),
                content: content_a,
            })
            .await;
        }

        let content_b = "/eom replan now".to_string();
        if let Some(args) = parse_eom(&content_b) {
            sink.handle_eom(EomCommand {
                thread_key: "k".into(),
                stream_id: None,
                topic: String::new(),
                sender_id: "7".into(),
                message_id: "2".into(),
                args,
            })
            .await;
        } else {
            sink.dispatch(ZulipDispatchedMessage {
                thread_key: "k".into(),
                stream_id: None,
                topic: String::new(),
                sender_id: "7".into(),
                message_id: "2".into(),
                content: content_b,
            })
            .await;
        }

        let dispatched = sink.log.lock().unwrap();
        let eom_called = sink.eom_log.lock().unwrap();
        assert_eq!(dispatched.len(), 1, "plain message routes to dispatch only");
        assert_eq!(
            eom_called.len(),
            1,
            "/eom message routes to handle_eom only"
        );
        assert_eq!(eom_called[0].args, "replan now");
    }

    // --- ZulipEmojiMap -----------------------------------------------------

    #[test]
    fn unicode_to_zulip_emoji_maps_hammer_and_wrench() {
        // U+1F6E0 (HAMMER AND WRENCH) + U+FE0F (VS-16) — the canonical default
        // glyph that ReactionEmojis::default().thinking will yield.
        assert_eq!(unicode_to_zulip_emoji("\u{1f6e0}\u{fe0f}"), "working_on_it");
    }

    #[test]
    fn default_thinking_emoji_resolves_to_a_known_name() {
        let thinking = crate::config::ReactionEmojis::default().thinking;
        assert_ne!(
            unicode_to_zulip_emoji(&thinking),
            "question",
            "thinking glyph {thinking:?} must map to a real Zulip emoji name"
        );
    }

    // --- ZulipTyping (stream + direct) -------------------------------------

    fn stream_channel() -> ChannelRef {
        ChannelRef {
            platform: "zulip".into(),
            channel_id: "42".into(),
            thread_id: Some("topic-x".into()),
            parent_id: None,
            origin_event_id: None,
        }
    }

    fn dm_channel() -> ChannelRef {
        ChannelRef {
            platform: "zulip".into(),
            channel_id: "[1234]".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: None,
        }
    }

    fn form_get<'a>(form: &'a [(&'a str, String)], key: &str) -> Option<&'a str> {
        form.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn typing_form_dm_uses_type_direct_and_to_array_literal() {
        let f_start = typing_form("start", &dm_channel());
        assert_eq!(form_get(&f_start, "op"), Some("start"));
        assert_eq!(form_get(&f_start, "type"), Some("direct"));
        assert_eq!(form_get(&f_start, "to"), Some("[1234]"));

        let f_stop = typing_form("stop", &dm_channel());
        assert_eq!(form_get(&f_stop, "op"), Some("stop"));
        assert_eq!(form_get(&f_stop, "type"), Some("direct"));
        assert_eq!(form_get(&f_stop, "to"), Some("[1234]"));
    }

    #[tokio::test]
    async fn stop_typing_dm_succeeds_on_200() {
        let canned = vec![Canned {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"success"}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        adapter.stop_typing(&dm_channel()).await.unwrap();
    }

    #[test]
    fn typing_form_stream_has_op_type_topic_and_stream_identifier() {
        let f = typing_form("start", &stream_channel());
        assert_eq!(form_get(&f, "op"), Some("start"));
        assert_eq!(form_get(&f, "type"), Some("stream"));
        assert_eq!(form_get(&f, "topic"), Some("topic-x"));
        // Stream identifier present as either `to` or `stream_id`.
        let to = form_get(&f, "to");
        let sid = form_get(&f, "stream_id");
        assert!(
            to == Some("42") || sid == Some("42"),
            "expected stream identifier 42, got to={to:?} stream_id={sid:?}"
        );
    }

    #[tokio::test]
    async fn start_typing_returns_err_on_http_500() {
        let canned = vec![Canned {
            status: 500,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"error","msg":"server"}"#.into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let err = adapter.start_typing(&stream_channel()).await.unwrap_err();
        assert!(err.to_string().contains("500"), "missing 500: {err}");
    }
}

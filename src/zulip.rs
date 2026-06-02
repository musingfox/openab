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

/// Build a Zulip `SenderContext` from a resolved sender name.
///
/// Mirrors `discord.rs` `build_sender_context`: a pure seam so name resolution
/// is unit-testable without HTTP. `full_name` is the event's `sender_full_name`
/// as `Option<&str>` — `None` (field missing) and `Some("")` (empty) both fall
/// back to `sender_id`, so `sender_name` and `display_name` are never empty.
fn build_sender_context(
    full_name: Option<&str>,
    sender_id: &str,
    channel_id: &str,
    thread_id: Option<&str>,
    message_id: &str,
    is_bot: bool,
) -> crate::adapter::SenderContext {
    let name = full_name.filter(|s| !s.is_empty()).unwrap_or(sender_id);
    crate::adapter::SenderContext {
        schema: "openab.sender.v1".into(),
        sender_id: sender_id.to_string(),
        sender_name: name.to_string(),
        display_name: name.to_string(),
        channel: "zulip".into(),
        channel_id: channel_id.to_string(),
        thread_id: thread_id.map(|s| s.to_string()),
        is_bot,
        timestamp: None,
        message_id: Some(message_id.to_string()),
        receiver_id: None,
    }
}

/// True if a Zulip `sender_email` belongs to a bot account.
///
/// Zulip bot accounts have an email whose local-part (the segment before `@`)
/// ends in `-bot` (e.g. `weather-bot@example.com`). Only the local-part is
/// inspected so a domain that merely contains `bot` (e.g. `alice@bot.example`)
/// is never misclassified as a bot.
fn email_is_bot(email: &str) -> bool {
    email.split('@').next().unwrap_or("").ends_with("-bot")
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

/// True if `content` carries an explicit user mention aimed at a *specific*
/// person (a `@**Name**` / `@_**Name**` token), excluding wildcard mentions
/// (`@**all|everyone|channel|stream|topic**`). Used so a bot does not
/// auto-follow a message that is explicitly addressed to someone else — an
/// `@**sibling**` handoff is the strongest "this isn't for me" signal, and it
/// fires on the very first handoff, before the sibling has posted in the topic.
fn mentions_specific_user(content: &str) -> bool {
    const WILDCARDS: &[&str] = &["all", "everyone", "channel", "stream", "topic"];
    // `@_**` (silent mention) does not contain the substring `@**`, so the two
    // markers are scanned independently without double-counting.
    for marker in ["@**", "@_**"] {
        let mut hay = content;
        while let Some(i) = hay.find(marker) {
            let after = &hay[i + marker.len()..];
            match after.find("**") {
                Some(end) => {
                    // strip the `|user_id` disambiguation suffix Zulip may add
                    let name = after[..end].split('|').next().unwrap_or("").trim();
                    if !name.is_empty() && !WILDCARDS.contains(&name) {
                        return true;
                    }
                    hay = &after[end + 2..];
                }
                None => break,
            }
        }
    }
    false
}

/// Decide whether to dispatch a user message under `allow_user_messages`.
///
/// `is_mentioned` (this bot was @-mentioned) and `is_dm` (a private message —
/// an implicit mention) short-circuit to `true`. Otherwise the mode decides:
///
/// - `Mentions` — never (an explicit mention was required and absent).
/// - `Involved` — only if the bot already participated in the topic.
/// - `MultibotMentions` — like `Involved`, but stays quiet when either a
///   sibling bot is already in the topic (`other_bot_present`) OR this message
///   explicitly @-mentions someone else (`directed_elsewhere`). The latter
///   closes the first-handoff gap: the topic's incumbent bot would otherwise
///   grab a message addressed to a sibling that has not posted yet.
///
/// `involved` / `other_bot_present` / `directed_elsewhere` are ignored when
/// `is_mentioned || is_dm`.
fn should_dispatch_user_message(
    mode: AllowUsers,
    is_mentioned: bool,
    is_dm: bool,
    involved: bool,
    other_bot_present: bool,
    directed_elsewhere: bool,
) -> bool {
    if is_mentioned || is_dm {
        return true;
    }
    match mode {
        AllowUsers::Mentions => false,
        AllowUsers::Involved => involved,
        AllowUsers::MultibotMentions => {
            involved && !other_bot_present && !directed_elsewhere
        }
    }
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

    /// Resolve this bot's own Zulip `user_id` via `GET /users/me`.
    ///
    /// Used by the `Involved` / `MultibotMentions` dispatch gates to recognize
    /// the bot's own past messages in a topic. Fetched once at adapter startup.
    pub async fn fetch_bot_user_id(&self) -> Result<u64> {
        let resp = self
            .api_call(reqwest::Method::GET, "/api/v1/users/me", None, None)
            .await?;
        resp.get("user_id")
            .and_then(|v| v.as_i64())
            .map(|i| i as u64)
            .ok_or_else(|| anyhow!("no user_id in /users/me response"))
    }

    /// Inspect recent history of a stream `topic` to drive the `Involved` /
    /// `MultibotMentions` dispatch gates. Returns `(involved, other_bot_present)`:
    ///
    /// - `involved` — this bot (`bot_user_id`) has posted in the topic before.
    /// - `other_bot_present` — some message was sent by a user in
    ///   `trusted_bot_ids` (a sibling bot sharing the stream).
    ///
    /// Fails closed: any HTTP/parse error yields `(false, false)`, so an
    /// unreachable history API degrades to mention-only behavior rather than
    /// over-responding.
    async fn topic_participation(
        &self,
        stream_id: &str,
        topic: &str,
        bot_user_id: u64,
        trusted_bot_ids: &HashSet<String>,
    ) -> (bool, bool) {
        let stream_num: u64 = match stream_id.parse() {
            Ok(n) => n,
            Err(_) => return (false, false),
        };
        let narrow = serde_json::json!([
            {"operator": "stream", "operand": stream_num},
            {"operator": "topic", "operand": topic},
        ])
        .to_string();
        let query: [(&str, String); 5] = [
            ("narrow", narrow),
            ("anchor", "newest".to_string()),
            ("num_before", "100".to_string()),
            ("num_after", "0".to_string()),
            ("apply_markdown", "false".to_string()),
        ];
        // This runs inside the event loop, so bound it hard — a slow/stuck
        // request must never freeze message processing. Fail closed on timeout
        // or error (degrade to mention-only rather than block the loop).
        let fetch = self.api_call(reqwest::Method::GET, "/api/v1/messages", None, Some(&query));
        let resp = match tokio::time::timeout(std::time::Duration::from_secs(15), fetch).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                debug!(error = %e, topic, "zulip topic history fetch failed; failing closed");
                return (false, false);
            }
            Err(_) => {
                warn!(topic, "zulip topic history fetch timed out (15s); failing closed");
                return (false, false);
            }
        };
        let mut involved = false;
        let mut other_bot_present = false;
        if let Some(msgs) = resp.get("messages").and_then(|v| v.as_array()) {
            for m in msgs {
                let Some(sid) = m.get("sender_id").and_then(|v| v.as_i64()) else {
                    continue;
                };
                if sid as u64 == bot_user_id {
                    involved = true;
                }
                if trusted_bot_ids.contains(&sid.to_string()) {
                    other_bot_present = true;
                }
            }
        }
        (involved, other_bot_present)
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

    /// Mark the topic resolved by prepending `✔ ` (U+2714 + ASCII space).
    /// Idempotent: if the topic already starts with `✔ `, the existing prefix
    /// is reused (no double-prefix). DMs (no topic) are a no-op.
    async fn resolve_topic(&self, channel: &ChannelRef, trigger_msg: &MessageRef) -> Result<()> {
        let Some(topic) = channel.thread_id.as_deref() else {
            // No topic — DM or otherwise topic-less. Nothing to resolve.
            return Ok(());
        };
        let unprefixed = topic.strip_prefix("\u{2714} ").unwrap_or(topic);
        let new_topic = format!("\u{2714} {unprefixed}");
        let form = [
            ("topic", new_topic),
            ("propagate_mode", "change_all".to_string()),
        ];
        let path = format!("/api/v1/messages/{}", trigger_msg.message_id);
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
    /// Sender's resolved full name (`sender_full_name` from the event), if the
    /// field was present and non-empty. `None` falls back to `sender_id` when
    /// building the `SenderContext`.
    pub sender_full_name: Option<String>,
    /// Zulip integer message ID as string (for reactions / streaming edits).
    pub message_id: String,
    /// Verbatim message body.
    pub content: String,
    /// Whether the sender is a bot account, classified from `sender_email`
    /// in the event loop (`email_is_bot`). Threaded into the `SenderContext`.
    pub is_bot: bool,
}

/// Trait surface for the dispatch side-effect: the event loop calls this for
/// every accepted message. Production wires this to the broker's `Dispatcher`;
/// unit tests use a recording double to assert thread-key correctness.
#[async_trait]
pub trait ZulipMessageSink: Send + Sync {
    async fn dispatch(&self, evt: ZulipDispatchedMessage);
}

/// Production sink: builds the broker `SenderContext` + `BufferedMessage` and
/// submits to the shared `Dispatcher`. Built once at startup and shared across
/// the event loop via `Arc`.
pub struct BrokerSink {
    adapter: Arc<ZulipAdapter>,
    dispatcher: Arc<crate::dispatch::Dispatcher>,
}

impl BrokerSink {
    pub fn new(
        adapter: Arc<ZulipAdapter>,
        dispatcher: Arc<crate::dispatch::Dispatcher>,
    ) -> Self {
        Self { adapter, dispatcher }
    }
}

#[async_trait]
impl ZulipMessageSink for BrokerSink {
    async fn dispatch(&self, evt: ZulipDispatchedMessage) {
        self.dispatch_impl(evt).await
    }
}

/// Pure build seam extracted from `BrokerSink::dispatch_impl`: turn a
/// `ZulipDispatchedMessage` (plus its already-classified `is_bot`) into the
/// `(thread_key, ChannelRef, BufferedMessage)` triple `Dispatcher::submit`
/// needs. No `Dispatcher`, `SessionPool`, or HTTP — so the SenderContext
/// construction (name resolution + `is_bot` flow) is unit-testable in isolation.
fn build_dispatch_parts(
    evt: ZulipDispatchedMessage,
    is_bot: bool,
) -> (String, ChannelRef, crate::dispatch::BufferedMessage) {
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

    // Resolve the display name from `sender_full_name` (falls back to the
    // numeric sender_id when missing/empty) via the pure seam.
    let sender = build_sender_context(
        evt.sender_full_name.as_deref(),
        &evt.sender_id,
        &channel_id,
        thread_id_opt.as_deref(),
        &evt.message_id,
        is_bot,
    );
    let resolved_name = sender.sender_name.clone();
    let sender_json = serde_json::to_string(&sender).unwrap_or_else(|_| "{}".into());
    let estimated_tokens = crate::dispatch::estimate_tokens(&evt.content, &[]);
    let buf = crate::dispatch::BufferedMessage {
        sender_json,
        sender_name: resolved_name,
        prompt: evt.content.clone(),
        extra_blocks: Vec::new(),
        trigger_msg,
        arrived_at: std::time::Instant::now(),
        estimated_tokens,
        other_bot_present: false,
    };
    (evt.thread_key.clone(), trigger_channel, buf)
}

impl BrokerSink {
    async fn dispatch_impl(&self, evt: ZulipDispatchedMessage) {
        let is_bot = evt.is_bot;
        let adapter_dyn: Arc<dyn ChatAdapter> = self.adapter.clone();
        let (thread_key, trigger_channel, buf) = build_dispatch_parts(evt, is_bot);
        if let Err(e) = self
            .dispatcher
            .submit(thread_key, trigger_channel, adapter_dyn, buf)
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

    // Resolve our own user_id up front for the Involved / MultibotMentions
    // dispatch gates (recognizing the bot's own past messages in a topic).
    // On failure, fall back to 0 — no real sender matches, so those modes
    // degrade to mention-only rather than over-responding.
    let bot_user_id = match adapter.fetch_bot_user_id().await {
        Ok(id) => {
            info!(bot_user_id = id, "zulip identity resolved");
            id
        }
        Err(e) => {
            warn!(error = %e, "zulip /users/me failed; involved/multibot gating degraded to mentions-only");
            0
        }
    };

    // Params still pending Zulip support; documented to silence dead-code.
    let _ = (
        &params.allow_bot_messages,
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
                let sender_full_name = body
                    .get("sender_full_name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let is_bot = body
                    .get("sender_email")
                    .and_then(|v| v.as_str())
                    .map(email_is_bot)
                    .unwrap_or(false);
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

                // Dispatch-mode gate: mentions / involved / multibot-mentions.
                // `flags` on the event carries "mentioned" when this bot was
                // @-mentioned; DMs (no stream) count as an implicit mention.
                let is_dm = stream_id.is_none();
                let is_mentioned = ev
                    .get("flags")
                    .and_then(|v| v.as_array())
                    .map(|fl| fl.iter().any(|f| f.as_str() == Some("mentioned")))
                    .unwrap_or(false);
                let (involved, other_bot_present) = if !is_mentioned
                    && !is_dm
                    && matches!(
                        params.allow_user_messages,
                        AllowUsers::Involved | AllowUsers::MultibotMentions
                    ) {
                    match &stream_id {
                        Some(sid) => {
                            adapter
                                .topic_participation(
                                    sid,
                                    &topic,
                                    bot_user_id,
                                    &params.trusted_bot_ids,
                                )
                                .await
                        }
                        None => (false, false),
                    }
                } else {
                    (false, false)
                };
                // An explicit @-mention of someone else (not us) means the
                // message is directed — don't auto-follow it as topic incumbent.
                let directed_elsewhere = !is_mentioned && mentions_specific_user(&content);
                if !should_dispatch_user_message(
                    params.allow_user_messages,
                    is_mentioned,
                    is_dm,
                    involved,
                    other_bot_present,
                    directed_elsewhere,
                ) {
                    debug!(
                        stream_id = ?stream_id,
                        topic = %topic,
                        is_mentioned,
                        involved,
                        other_bot_present,
                        directed_elsewhere,
                        "zulip dispatch-mode gate: skip"
                    );
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
                sink.dispatch(ZulipDispatchedMessage {
                    thread_key: key,
                    stream_id,
                    topic,
                    sender_id,
                    sender_full_name,
                    message_id,
                    content,
                    is_bot,
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

    // --- should_dispatch_user_message ---

    #[test]
    fn dispatch_mention_always_wins() {
        for mode in [
            AllowUsers::Mentions,
            AllowUsers::Involved,
            AllowUsers::MultibotMentions,
        ] {
            assert!(should_dispatch_user_message(
                mode, true, false, false, false, false
            ));
        }
    }

    #[test]
    fn dispatch_dm_always_wins() {
        for mode in [
            AllowUsers::Mentions,
            AllowUsers::Involved,
            AllowUsers::MultibotMentions,
        ] {
            assert!(should_dispatch_user_message(
                mode, false, true, false, false, false
            ));
        }
    }

    #[test]
    fn dispatch_mentions_mode_requires_mention() {
        // Even when involved, Mentions mode stays quiet without an @-mention.
        assert!(!should_dispatch_user_message(
            AllowUsers::Mentions,
            false,
            false,
            true,
            false,
            false
        ));
    }

    #[test]
    fn dispatch_involved_follows_topic_ignoring_other_bots() {
        assert!(should_dispatch_user_message(
            AllowUsers::Involved,
            false,
            false,
            true,
            false,
            false
        ));
        assert!(!should_dispatch_user_message(
            AllowUsers::Involved,
            false,
            false,
            false,
            false,
            false
        ));
        // Other bots are irrelevant in plain Involved mode.
        assert!(should_dispatch_user_message(
            AllowUsers::Involved,
            false,
            false,
            true,
            true,
            false
        ));
    }

    #[test]
    fn dispatch_multibot_requires_mention_when_sibling_present() {
        // Single bot in the topic: auto-follow.
        assert!(should_dispatch_user_message(
            AllowUsers::MultibotMentions,
            false,
            false,
            true,
            false,
            false
        ));
        // Sibling bot present + no mention: stay quiet.
        assert!(!should_dispatch_user_message(
            AllowUsers::MultibotMentions,
            false,
            false,
            true,
            true,
            false
        ));
        // Never participated: quiet.
        assert!(!should_dispatch_user_message(
            AllowUsers::MultibotMentions,
            false,
            false,
            false,
            false,
            false
        ));
    }

    #[test]
    fn dispatch_multibot_backs_off_when_directed_elsewhere() {
        // Topic incumbent (involved, no sibling has posted yet) but the message
        // explicitly @-mentions someone else: stay quiet — closes the
        // first-handoff gap where the incumbent grabbed a sibling's message.
        assert!(!should_dispatch_user_message(
            AllowUsers::MultibotMentions,
            false,
            false,
            true,
            false,
            true
        ));
        // A directed mention OF this bot still short-circuits to act
        // (is_mentioned wins; directed_elsewhere is only set when !is_mentioned).
        assert!(should_dispatch_user_message(
            AllowUsers::MultibotMentions,
            true,
            false,
            true,
            false,
            false
        ));
    }

    #[test]
    fn mentions_specific_user_detects_directed_handoff() {
        assert!(mentions_specific_user("@**dev** 可以建立一個新 agent 嗎"));
        assert!(mentions_specific_user("cc @_**invest** 看一下"));
        assert!(mentions_specific_user("ping @**Full Name|1086906** now"));
        // wildcard mentions are not "directed at a specific person"
        assert!(!mentions_specific_user("@**all** heads up"));
        assert!(!mentions_specific_user("@**everyone** sync"));
        // plain text / no mention
        assert!(!mentions_specific_user("just a normal follow-up message"));
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

    // --- ZulipSenderContextSeam -------------------------------------------

    #[test]
    fn zulip_build_sender_context_resolves_display_name() {
        let ctx = build_sender_context(Some("Alice Wu"), "7", "42", Some("x"), "1", false);
        assert_eq!(ctx.display_name, "Alice Wu");
        assert_eq!(ctx.sender_name, "Alice Wu");
        assert_eq!(ctx.sender_id, "7");
    }

    #[test]
    fn zulip_build_sender_context_falls_back_to_sender_id_when_name_missing() {
        // Missing field (None) falls back to the numeric sender_id.
        let missing = build_sender_context(None, "7", "42", Some("x"), "1", false);
        assert_eq!(missing.display_name, "7");
        assert_eq!(missing.sender_name, "7");
        // Empty string falls back too (never an empty display name).
        let empty = build_sender_context(Some(""), "7", "42", Some("x"), "1", false);
        assert_eq!(empty.display_name, "7");
        assert_eq!(empty.sender_name, "7");
    }

    /// A1: `build_sender_context` propagates the `is_bot` argument into the
    /// returned `SenderContext.is_bot` in both directions (so a hardcoded
    /// constant cannot pass).
    #[test]
    fn zulip_build_sender_context_propagates_is_bot() {
        let bot = build_sender_context(Some("Weather Bot"), "9", "42", Some("x"), "1", true);
        assert!(bot.is_bot, "is_bot=true must yield ctx.is_bot == true");
        let human = build_sender_context(Some("Alice Wu"), "7", "42", Some("x"), "1", false);
        assert!(!human.is_bot, "is_bot=false must yield ctx.is_bot == false");
    }

    #[test]
    fn email_is_bot_classifies_local_part_suffix() {
        assert!(email_is_bot("weather-bot@example.com"));
        assert!(!email_is_bot("alice@example.com"));
        // Domain containing "bot" must NOT be misclassified — only local-part counts.
        assert!(!email_is_bot("alice@bot.example.com"));
        assert!(!email_is_bot(""));
    }

    // --- ZulipDispatchSeam (A3 + D1) --------------------------------------
    // SenderContext is Serialize-only (no Deserialize), so test bodies parse
    // the produced `sender_json` as a generic `serde_json::Value` and index it.

    /// Build a `ZulipDispatchedMessage` for the seam tests.
    fn seam_evt(sender_full_name: &str, sender_id: &str, is_bot: bool) -> ZulipDispatchedMessage {
        ZulipDispatchedMessage {
            thread_key: "zulip:stream:42:x".into(),
            stream_id: Some("42".into()),
            topic: "x".into(),
            sender_id: sender_id.into(),
            sender_full_name: Some(sender_full_name.into()),
            message_id: "1".into(),
            content: "hi".into(),
            is_bot,
        }
    }

    /// A3: a bot sender taken through the production build seam serializes to
    /// `sender_json` with `is_bot == true` — proving is_bot reached production
    /// SenderContext rather than the old hardcoded `false`.
    #[test]
    fn dispatch_impl_seam_serializes_is_bot_true_for_bot_sender() {
        let evt = seam_evt("Weather Bot", "9", true);
        let (_thread_key, _channel, buf) = build_dispatch_parts(evt, true);
        let v: serde_json::Value = serde_json::from_str(&buf.sender_json).unwrap();
        assert_eq!(
            v["is_bot"].as_bool(),
            Some(true),
            "bot sender must serialize is_bot == true: {}",
            buf.sender_json
        );
    }

    /// D1 (HOLE1 regression guard): the seam preserves the resolved sender full
    /// name. With sender_full_name="Alice Wu", sender_id="7", buf.sender_name
    /// must be "Alice Wu" (not the id "7"), and the serialized display_name too.
    /// Reverting sender_name back to sender_id makes this go RED.
    #[test]
    fn dispatch_impl_seam_preserves_resolved_sender_name() {
        let evt = seam_evt("Alice Wu", "7", false);
        let (_thread_key, _channel, buf) = build_dispatch_parts(evt, false);
        assert_eq!(buf.sender_name, "Alice Wu");
        assert_ne!(buf.sender_name, "7");
        let v: serde_json::Value = serde_json::from_str(&buf.sender_json).unwrap();
        assert_eq!(v["display_name"].as_str(), Some("Alice Wu"));
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
        let (url, _recorded) = spawn_mock_recording(canned).await;
        url
    }

    /// Variant of `spawn_mock` that captures the full request string (headers
    /// and body) for each handled connection. Returns `(base_url, recorded)`;
    /// tests inspect `recorded` to assert request shape — method, path, form
    /// fields. Body-aware: reads the declared `Content-Length` bytes.
    async fn spawn_mock_recording(canned: Vec<Canned>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let rec_clone = recorded.clone();
        tokio::spawn(async move {
            for c in canned {
                let (mut sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let mut buf = [0u8; 4096];
                let mut total = String::new();
                // Read until headers complete.
                while let Ok(n) = sock.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    total.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if total.contains("\r\n\r\n") {
                        break;
                    }
                }
                // If a Content-Length is declared, read the remaining body bytes.
                let header_end = total.find("\r\n\r\n").map(|i| i + 4).unwrap_or(total.len());
                let content_length: usize = total[..header_end]
                    .lines()
                    .find_map(|l| {
                        let mut parts = l.splitn(2, ':');
                        let k = parts.next()?.trim();
                        let v = parts.next()?.trim();
                        if k.eq_ignore_ascii_case("content-length") {
                            v.parse().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let already = total.len() - header_end;
                if content_length > already {
                    let need = content_length - already;
                    let mut got = 0;
                    while got < need {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                total.push_str(&String::from_utf8_lossy(&buf[..n]));
                                got += n;
                            }
                        }
                    }
                }
                rec_clone.lock().unwrap().push(total);
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
        (format!("http://{addr}"), recorded)
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

    // --- ZulipResolveTopic ---

    fn ok_200() -> Canned {
        Canned {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"success"}"#.into(),
        }
    }

    fn resolve_channel(topic: &str) -> ChannelRef {
        ChannelRef {
            platform: "zulip".into(),
            channel_id: "42".into(),
            thread_id: Some(topic.into()),
            parent_id: None,
            origin_event_id: None,
        }
    }

    fn resolve_trigger(message_id: &str, channel: ChannelRef) -> MessageRef {
        MessageRef {
            channel,
            message_id: message_id.into(),
        }
    }

    #[tokio::test]
    async fn resolve_topic_prepends_check_mark_and_propagate_all() {
        let (base, recorded) = spawn_mock_recording(vec![ok_200()]).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let channel = resolve_channel("Bug X");
        let msg = resolve_trigger("42", channel.clone());
        adapter.resolve_topic(&channel, &msg).await.unwrap();

        let reqs = recorded.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];
        // Method + path on the request line.
        assert!(
            req.starts_with("PATCH /api/v1/messages/42 "),
            "expected PATCH on message 42, got: {}",
            req.lines().next().unwrap_or("")
        );
        // Form body should carry URL-encoded check-mark + topic + propagate_mode.
        // U+2714 = E2 9C 94 → %E2%9C%94
        assert!(
            req.contains("topic=%E2%9C%94+Bug+X") || req.contains("topic=%E2%9C%94%20Bug%20X"),
            "missing topic=✔ Bug X (url-encoded), body: {req}"
        );
        assert!(
            req.contains("propagate_mode=change_all"),
            "missing propagate_mode=change_all, body: {req}"
        );
    }

    #[tokio::test]
    async fn resolve_topic_is_idempotent_on_already_resolved() {
        let (base, recorded) = spawn_mock_recording(vec![ok_200()]).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let channel = resolve_channel("\u{2714} Bug X");
        let msg = resolve_trigger("42", channel.clone());
        adapter.resolve_topic(&channel, &msg).await.unwrap();

        let reqs = recorded.lock().unwrap().clone();
        let req = &reqs[0];
        // Must NOT double-prefix — body should contain exactly one ✔ (encoded).
        let occurrences = req.matches("%E2%9C%94").count();
        assert_eq!(
            occurrences, 1,
            "expected single ✔ prefix, got {occurrences} in body: {req}"
        );
        assert!(req.contains("Bug+X") || req.contains("Bug%20X"));
    }

    #[tokio::test]
    async fn resolve_topic_surfaces_permission_error() {
        let canned = vec![Canned {
            status: 400,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"error","code":"BAD_REQUEST","msg":"You don't have permission"}"#
                .into(),
        }];
        let base = spawn_mock(canned).await;
        let adapter = ZulipAdapter::new(base, "b@x", "k");
        let channel = resolve_channel("Bug X");
        let msg = resolve_trigger("42", channel.clone());
        let err = adapter.resolve_topic(&channel, &msg).await.unwrap_err();
        let s = err.to_string();
        assert!(s.contains("400"), "missing status: {s}");
    }

    #[tokio::test]
    async fn resolve_topic_dm_no_topic_is_noop() {
        // No mock canned: if it tried HTTP, the test would hang/fail.
        let adapter = ZulipAdapter::new("http://127.0.0.1:1", "b@x", "k");
        let channel = ChannelRef {
            platform: "zulip".into(),
            channel_id: "[7,11]".into(),
            thread_id: None,
            parent_id: None,
            origin_event_id: None,
        };
        let msg = resolve_trigger("42", channel.clone());
        adapter.resolve_topic(&channel, &msg).await.unwrap();
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
        /// Resolved sender name (from the seam) so tests can observe that the
        /// full name flowed through the dispatch path instead of the numeric id.
        sender_name: String,
        /// Bot classification (from `email_is_bot`) observed at the sink so the
        /// event loop's sender_email → is_bot path can be asserted end-to-end.
        is_bot: bool,
        #[allow(dead_code)]
        content: String,
    }

    /// Recording sink — records every dispatched event for assertions.
    struct RecordingSink {
        log: Mutex<Vec<DispatchedEvent>>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                log: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ZulipMessageSink for RecordingSink {
        async fn dispatch(&self, evt: ZulipDispatchedMessage) {
            // Resolve through the same seam production uses, so the recorded
            // name reflects the real dispatch-path resolution.
            let ctx = build_sender_context(
                evt.sender_full_name.as_deref(),
                &evt.sender_id,
                evt.stream_id.as_deref().unwrap_or_default(),
                if evt.topic.is_empty() {
                    None
                } else {
                    Some(evt.topic.as_str())
                },
                &evt.message_id,
                evt.is_bot,
            );
            self.log.lock().unwrap().push(DispatchedEvent {
                thread_key: evt.thread_key,
                stream_id: evt.stream_id,
                sender_id: evt.sender_id,
                sender_name: ctx.sender_name,
                is_bot: ctx.is_bot,
                content: evt.content,
            });
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

    /// Canned `GET /users/me` reply that `run_zulip_adapter` consumes once at
    /// startup (before the register/poll loop) to learn its own user_id.
    fn users_me_ok() -> Canned {
        Canned {
            status: 200,
            headers: vec![("Content-Type", "application/json".into())],
            body: r#"{"result":"success","user_id":999}"#.into(),
        }
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
            users_me_ok(),
            // /register response
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","queue_id":"q1","last_event_id":-1}"#.into(),
            },
            // /events first poll — one @-mention message event
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","events":[{"id":1,"type":"message","flags":["mentioned"],"message":{"stream_id":42,"subject":"x","sender_id":7,"content":"hi"}}]}"#.into(),
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

    /// E1: a stream event carrying `sender_full_name:"Alice Wu"` must reach the
    /// sink with the resolved name "Alice Wu" (not the numeric sender_id "7").
    #[tokio::test(flavor = "current_thread")]
    async fn event_loop_dispatches_resolved_sender_full_name() {
        let canned = vec![
            users_me_ok(),
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","queue_id":"q1","last_event_id":-1}"#.into(),
            },
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","events":[{"id":1,"type":"message","flags":["mentioned"],"message":{"stream_id":42,"subject":"x","sender_id":7,"sender_full_name":"Alice Wu","content":"hi"}}]}"#.into(),
            },
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

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = tx.send(true);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("loop should exit within 5s")
            .expect("task should join");
        res.expect("loop should return Ok");

        let log = sink.log.lock().unwrap();
        assert_eq!(log.len(), 1, "expected 1 dispatched message");
        assert_eq!(log[0].sender_name, "Alice Wu");
        assert_ne!(log[0].sender_name, "7");
    }

    /// A2: the event loop classifies a `sender_email` ending `-bot` as a bot
    /// and a plain user email as a human. Two events in one poll → the bot
    /// event records `is_bot == true`, the human event `is_bot == false`.
    /// The human→false assertion blocks a hardcoded `true`.
    #[tokio::test(flavor = "current_thread")]
    async fn event_loop_classifies_bot_email_sender_as_bot() {
        let canned = vec![
            users_me_ok(),
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","queue_id":"q1","last_event_id":-1}"#.into(),
            },
            // One poll, two @-mention message events: a bot then a human.
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","events":[{"id":1,"type":"message","flags":["mentioned"],"message":{"stream_id":42,"subject":"x","sender_id":7,"sender_email":"weather-bot@example.com","content":"hi"}},{"id":2,"type":"message","flags":["mentioned"],"message":{"stream_id":42,"subject":"x","sender_id":8,"sender_email":"alice@example.com","content":"yo"}}]}"#.into(),
            },
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

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = tx.send(true);
        let res = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("loop should exit within 5s")
            .expect("task should join");
        res.expect("loop should return Ok");

        let log = sink.log.lock().unwrap();
        assert_eq!(log.len(), 2, "expected 2 dispatched messages");
        assert!(
            log[0].is_bot,
            "weather-bot@example.com must be classified as a bot"
        );
        assert!(
            !log[1].is_bot,
            "alice@example.com must be classified as a human"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn event_loop_drops_event_outside_allowlist() {
        let canned = vec![
            users_me_ok(),
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
            users_me_ok(),
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
            // /events on new queue: one allowed @-mention message
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","events":[{"id":1,"type":"message","flags":["mentioned"],"message":{"stream_id":42,"subject":"y","sender_id":7,"content":"hi"}}]}"#.into(),
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
            users_me_ok(),
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
            users_me_ok(),
            Canned {
                status: 200,
                headers: vec![("Content-Type", "application/json".into())],
                body: r#"{"result":"success","queue_id":"q1","last_event_id":-1}"#.into(),
            },
            // No further response → the loop will be parked in poll_events.
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

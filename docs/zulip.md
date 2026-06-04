# Zulip Adapter

Connect openab to a Zulip workspace alongside Discord/Slack. The adapter
long-polls the Zulip events API and pushes messages through the same ACP
session pool as the other adapters.

> **Bot type is load-bearing.** You **must** use a Zulip **generic bot**.
> Outgoing-webhook bots cannot edit messages or add reactions, both of which
> the streaming-status protocol requires (see Troubleshooting #2). Do **NOT
> use** an `outgoing.webhook` bot.

## Prerequisites

- A Zulip workspace where you are an organisation admin (or have a teammate
  who is — only admins can create bots).
- Docker installed and running on the operator host.
- This is a fork of upstream openab. The image is built locally, so you must
  run `docker compose build` once before the first `docker compose up`, and
  again after pulling new upstream changes. (This is the D8 image-delivery
  prerequisite: the operator's `compose.yml` consumes a `build:` directive
  pointing at `./build/openab` rather than pulling a registry image.)

## 1. Create the bot in Zulip

1. Open Zulip → **Settings (gear)** → **Personal** → **Bots** (or **Organization → Bots** if you're an admin).
2. Click **Add a new bot**.
3. **Bot type**: choose **Generic bot**. Do **NOT** pick `Outgoing webhook` — it cannot edit messages.
4. **Full name**: anything (e.g. "openab").
5. **Username**: e.g. `openab-bot` (this becomes part of the bot's email).
6. Click **Create bot**.

## 2. Copy credentials

From the bots list:

1. **Email** — looks like `openab-bot@your-org.zulipchat.com`. This is the API username.
2. **API key** — click "Download zuliprc" or click the API key field to reveal it. This is the API password.
3. **Site URL** — your workspace URL. Examples:
   - Zulip Cloud: `https://your-org.zulipchat.com`
   - Self-hosted: `https://zulip.example.com`

Subscribe the bot to the streams you want it active in (otherwise it won't see
events).

## 3. Fill in `.env`

In `/Users/nickhuang/openab-host/.env` (chmod 600), set:

```sh
ZULIP_BOT_EMAIL=openab-bot@your-org.zulipchat.com
ZULIP_API_KEY=...your_key_here...
ZULIP_SITE=https://your-org.zulipchat.com
```

`ZULIP_SITE` is required and has no default — Zulip Cloud and self-hosted
both require an explicit URL.

## 4. Add `[zulip]` to `config.toml`

```toml
[zulip]
site = "${ZULIP_SITE}"
bot_email = "${ZULIP_BOT_EMAIL}"
api_key = "${ZULIP_API_KEY}"

# Allowlist v1: numeric stream IDs / user IDs as strings (matches Slack/Discord
# pattern). No `allowed_topics` in v1 — a topic rename forks the session, see
# Troubleshooting #3 below.
allowed_channels = ["42"]   # numeric stream_id allowed_channels example
allowed_users = ["7"]       # numeric user_id allowed_users example
```

Find the numeric `stream_id`: open the stream in Zulip's web UI, click the
gear/options menu, choose "Stream settings"; the ID is in the URL
(`#streams/<stream_id>/...`) or visible in the settings panel.

Find a numeric `user_id`: open the user's profile menu in any conversation —
the ID is shown in the profile pane.

## 5. Build and start

```sh
docker compose build       # first time, and after upstream changes
docker compose up -d
docker compose logs -f openab
```

You should see in the logs:

```
INFO openab::zulip: starting zulip adapter
INFO openab::zulip: zulip event queue registered queue_id=...
```

Post a message in an allowed stream/topic where the bot is subscribed; the
adapter dispatches the event through the broker.

## 6. Reload after config changes

```sh
docker compose restart openab
```

The adapter re-registers its event queue cleanly.

## Troubleshooting

1. **`missing field api_key` at startup.** Your `.env` doesn't have
   `ZULIP_API_KEY`, or the variable expanded to an empty string. Verify with
   `grep ZULIP_API_KEY /Users/nickhuang/openab-host/.env` then
   `docker compose restart openab`.

2. **`HTTP 403` on `update_message` (or on reactions).** Your bot is an
   **outgoing webhook**, not a generic bot. Outgoing webhooks cannot edit
   messages or react. Create a new **Generic bot** (Section 1 above), replace
   the credentials in `.env`, and restart.

3. **A topic rename in Zulip created a "new" conversation in the agent.** This
   is a known v1 limitation. The adapter's thread key is
   `zulip:stream:{stream_id}:{normalized_topic}` (lowercased + trimmed). When a
   user renames the topic, subsequent events carry the new topic and therefore
   a new session. The old session is left intact (no data loss) but the agent
   starts fresh in the renamed topic. Workaround: avoid topic renames in
   long-running bot conversations.

4. **Allowlist denied a message silently.** The adapter logs at `debug` when
   an event is dropped by the channel/user allowlist. Enable debug logs:
   `RUST_LOG=openab::zulip=debug docker compose up -d`. Then check that the
   `stream_id` / `sender_id` you expected match an entry in
   `allowed_channels` / `allowed_users` exactly (both are numeric strings).

## Limitations (v1)

- No `allowed_topics` — permission granularity is stream-level only. Add per-topic
  allowlisting if it becomes operationally painful (additive, non-breaking).
- Topic renames fork sessions (see Troubleshooting #3).
- User display names are not resolved in v1 — the prompt SenderContext sees
  the numeric `sender_id` verbatim. Agents that pretty-print sender names
  will show numbers instead of names.
- `allow_user_messages = "involved"` is parsed but not yet enforced by the Zulip
  event loop (review advisory #4). All allowlisted messages dispatch; "involved"
  thread tracking lands in v1.1.
- Bot self-suppression not yet wired (review advisory #5). Workaround:
  set `allowed_users` to an explicit list that EXCLUDES the bot's own user_id,
  so the existing allowlist gate drops the bot's own messages. Default
  `allowed_users = []` (= `allow_all_users`) **will** cause an echo loop once the
  bot is subscribed to the stream — see Troubleshooting #4.

## Topic auto-resolve (`[[resolve]]` directive)

When the agent emits `[[resolve]]` as the **first line** of its final reply,
the broker strips that line from the visible message and — only if the turn
completed naturally (id-bearing-success notification, not EOF / timeout /
error) — issues `PATCH /api/v1/messages/{id}` with
`topic=✔ <original topic>` and `propagate_mode=change_all`. Zulip then
renames the topic and visually collapses it under its resolved-topic UX.

### Agent-side system-prompt snippet

To opt the agent into this behaviour, include a line like this in your
agent's system prompt (or `opencode.json` instructions):

> When the user has confirmed the task is complete and the conversation is
> done, emit `[[resolve]]` as the first line of your final response.

### Required Zulip permission

The bot user must belong to a group permitted to rename topics. The exact
setting name depends on your Zulip server version:

- Zulip ≥ 10.0: `can_resolve_topics_group` (preferred)
- Earlier Zulip: `can_move_messages_between_topics_group`

If neither is granted, the PATCH returns HTTP 400 with a permission error;
see Failure mode below.

### Failure mode

If the PATCH fails (permission, network, etc.) the broker logs a `warn` with
the underlying error and the turn **completes normally** — the agent's reply
is still posted and the success reaction still fires. The user sees no
warning; only operators tailing the broker log see the failure. Fix the
permission or unblock the bot, then the next `[[resolve]]` will succeed.

### Idempotency

If the topic already starts with `✔ ` (e.g., it was resolved earlier and the
agent emits `[[resolve]]` again), the broker reuses the existing prefix
instead of stacking — the PATCH carries the unchanged `✔ <name>` rather than
`✔ ✔ <name>`. Calling `[[resolve]]` on an already-resolved topic is safe.

### Side effect: thread key forks

A topic rename changes the Zulip topic name, which is part of the broker's
session key (see "Limitations (v1)" #3). The ACP session anchored at the
old topic is implicitly retired — subsequent messages on the renamed topic
will start a fresh session. Since `[[resolve]]` is emitted at end-of-turn
this is generally the intended outcome (the conversation is, after all,
resolved); it is documented here so the behaviour is not surprising.

## Topic follow/mute (`[[follow]]` / `[[mute]]` directives)

When the agent emits `[[follow]]` or `[[mute]]` as the **first line** of its
reply, the broker strips that line from the visible message and immediately
sets the bot user's Zulip topic visibility policy via
`POST /api/v1/user_topics` (`visibility_policy`: 3 = Follow, 1 = Mute).

Unlike `[[resolve]]`, these directives fire **unconditionally** — they are
not gated on natural turn completion. If the directive is present, the API
call is made regardless of how the turn ended.

### Agent-side system-prompt snippet

To opt the agent into topic visibility control, include a line like these in
your agent's system prompt (or `opencode.json` instructions):

> When starting a long-running task that will span multiple turns, emit
> `[[follow]]` as the first line of your first reply to follow the topic and
> ensure notifications are delivered.

> When a noisy topic should be silenced, emit `[[mute]]` as the first line of
> your reply to mute the topic for the bot user.

The directives are orthogonal and can coexist with `[[resolve]]` — for
example `[[resolve]]` on one line and `[[follow]]` on the next sets both.
Last directive wins if `[[follow]]` and `[[mute]]` both appear (e.g.
`[[follow]]` then `[[mute]]` → Mute).

### Required Zulip permission

The bot user must be able to call `POST /api/v1/user_topics`. This endpoint
controls per-user topic preferences and is available to all authenticated
users by default — no special group membership is required.

### Failure mode

If the API call fails (network, unexpected server error, etc.) the broker
logs a `warn` with the underlying error and the turn **completes normally** —
the agent's reply is still posted. The visibility policy is left unchanged.
Only operators tailing the broker log see the failure. Retry by prompting the
agent to emit the directive again on its next reply.

### Platform scope

`[[follow]]` and `[[mute]]` are Zulip-only directives. On Discord, Slack, and
other platforms the `set_topic_visibility` trait method is a default no-op —
the directive is silently ignored.

## Cross-topic links (`#**stream>topic**` syntax)

Zulip supports a native markdown link syntax for referencing another stream's
topic: `#**stream>topic**`. When a message containing this literal is posted,
the Zulip server renders it as a clickable link that navigates directly to that
topic's conversation. No special adapter support is required — the text passes
through the broker unchanged and Zulip handles rendering on the server side.

The agent should use `#**stream>topic**` whenever it wants to point the user to
a conversation in a different stream or topic — for example, to link back to a
related issue in `#**ops>incident-2025-06-01**` or to hand off to a different
team's stream.

### Agent-side system-prompt snippet

To teach the agent this syntax, include a snippet like the following in your
agent's system prompt (or `opencode.json` instructions field):

> When referring to a conversation in another Zulip stream or topic, use the
> native link syntax `#**stream>topic**` — for example
> `#**general>deploy**`. Zulip renders this as a clickable link on the server;
> no extra formatting is needed.

Paste this into the agent's system prompt via your operator config (e.g. the
`instructions` field in `opencode.json`) for it to take effect. Without that
human-in-the-loop injection the agent will not know to use the syntax.

## Known gaps for v1.1

Tracked here so they don't get rediscovered.

### Slash command UX integration

Zulip does **not** expose any API for third-party bots to register slash
commands in the compose-box autocomplete (`/me`, `/poll`, `/todo` etc. are
hardcoded in the Zulip server). Any future text-prefix commands (e.g.
`/help`, `/reset`) would work but get no UI affordance — users must know to
type them.

Workarounds, none ideal:

1. Implement a `/help` text-prefix command (~20 LOC) that returns a markdown
   list of available bot commands. Industry standard for Zulip bots —
   `zulip_bots` framework defaults to this.
2. Edit the bot's Zulip `full_name` to embed a hint. Visible on hover;
   cluttered in message list.
3. Bot posts an inline tip on first @mention per topic. Annoying on repeat.
4. Zulip admin sets a linkifier to highlight command text — visual hint only,
   no autocomplete. Affects ALL messages, not bot-specific.
5. Fork Zulip server itself. Only viable for self-hosted; massive effort;
   not pursued.

If/when `/help` lands, link from here and from the Troubleshooting section.

### Other deferred items

- `/reset` and `/cancel` parity with the Discord/Slack adapters
  (they have native slash command versions; Zulip needs the text-prefix shape).
- HTTP-400 fallback detection for `BAD_EVENT_QUEUE_ID` uses substring match
  (review advisory #2). Extract a sentinel constant shared by `api_call`'s
  formatter and `classify_events_error` so a refactor of one breaks the other
  at compile time.
- TCP-mock test helper depends on `Connection: close` to avoid reqwest
  keep-alive reuse (review advisory #3). Test infra concern only.

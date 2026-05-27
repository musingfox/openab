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

## Known gaps for v1.1

Tracked here so they don't get rediscovered.

### Slash command UX integration

Zulip does **not** expose any API for third-party bots to register slash
commands in the compose-box autocomplete (`/me`, `/poll`, `/todo` etc. are
hardcoded in the Zulip server). The text-prefix commands (`/eom` today,
likely `/help` and `/reset` later) work but get no UI affordance — users
must know to type them.

Workarounds, none ideal:

1. Implement a `/help` text-prefix command (~20 LOC, same shape as `/eom`)
   that returns a markdown list of available bot commands. Industry standard
   for Zulip bots — `zulip_bots` framework defaults to this.
2. Edit the bot's Zulip `full_name` to embed a hint, e.g.
   `adam-bot (try /eom <task>)`. Visible on hover; cluttered in message list.
3. Bot posts an inline tip on first @mention per topic. Annoying on repeat.
4. Zulip admin sets a linkifier to highlight `/eom` text — visual hint only,
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

# Goal-Driven Agent Loop

Design spec for a goal-oriented execution mode where agents work autonomously until a defined objective is achieved.

## Problem

Today, agents respond to individual messages reactively. There is no mechanism to assign a persistent **goal** that agents must work toward across multiple rounds, self-organizing their approach without explicit step-by-step instructions.

## Concept: "Escape Room" Mode

The human sets a goal and a success condition. A CronJob periodically evaluates whether the goal is met. If not, it posts to the channel — agents must **self-organize** to figure out how to achieve it. They are not told what to do, only what the goal is and that it hasn't been met yet.

```
Human sets goal via cron config
         │
         ▼
┌──► CronJob fires (on schedule)
│         │
│         ▼
│    Run disable_on_success command
│         │
│    ┌────┴─────┐
│    │ exit 0?  │
│    └────┬─────┘
│     No  │  Yes
│     │   │    │
│     ▼   │    ▼
│  Send message:     Goal achieved ✅
│  agents continue   Auto-disable job
│  working           (no message sent)
│         │
│         ▼
│  Agents discuss & act
│  (self-organized)
│         │
└─────────┘
     Next schedule
```

---

# Phase 1: Cron Auto-Disable on Success (MVP)

Minimal extension to existing `[[cron.jobs]]` — add a single field `disable_on_success`.

## Configuration

```toml
[[cron.jobs]]
id = "unit-tests-pass"                            # REQUIRED for disable_on_success jobs
schedule = "*/10 * * * *"
channel = "123456789012345678"
thread_id = ""                                    # optional: auto-created on first fire if empty
message = "Goal not met: all unit tests must pass. <@&1496247626675257384> please continue."
disable_on_success = "npm test"                   # NEW: command to evaluate goal
disable_on_success_timeout_secs = 60              # NEW: command timeout
disable_on_success_working_dir = "/repo"          # NEW: working directory for command
generation = 1                                    # NEW: bump to re-enable after auto-disable
```

### New Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `id` | ✅ (when `disable_on_success` set) | — | Stable unique identifier for state persistence |
| `disable_on_success` | | — | Shell command; if exit 0, job auto-disables and message is NOT sent |
| `disable_on_success_timeout_secs` | | `60` | Max seconds for command to run before being killed |
| `disable_on_success_working_dir` | | — | Working directory for command execution |
| `generation` | | `1` | Bump this number to re-enable an auto-disabled job |

### Behavior

When a cron job has `disable_on_success` set:

1. Schedule fires
2. Check: if persisted `generation` matches config `generation` AND job is auto-disabled → skip (stay disabled)
3. If config `generation` > persisted `generation` → reset auto-disable state (re-enabled by human)
4. Run `disable_on_success` command (with `disable_on_success_timeout_secs` and `disable_on_success_working_dir`)
5. **exit 0** → Goal achieved. Persist auto-disabled state with current `generation`. Do NOT send message.
6. **exit != 0** → Goal not met. Send `message` to channel/thread as normal.
7. **timeout exceeded** → Treat as exit != 0 (goal not met). Send message.

### Thread Lifecycle

| Scenario | Behavior |
|----------|----------|
| `thread_id` provided | Use that thread for all fires |
| `thread_id` empty | Auto-create a thread on first fire; persist `thread_id` in state |

All messages go to the **same thread** — agents need conversation history as context.

### Persistence

Auto-disable state must survive restarts. Persisted per job (keyed by `id`):

```json
{
  "unit-tests-pass": {
    "generation": 1,
    "auto_disabled": true,
    "auto_disabled_at": "2026-05-13T22:00:00Z",
    "thread_id": "1504239931940409587"
  }
}
```

Storage: **local JSON state file** (`cron-state.json`) — loaded on startup, written on state change.

**Re-enable logic:** When config `generation` > persisted `generation`, the auto-disable is cleared and the job runs again. This gives humans a clear, unambiguous way to restart a completed goal — just bump the number.

### Security

`disable_on_success` executes arbitrary shell commands. MVP mitigation:

- Trust config source (same model as existing `[[cron.jobs]]` message execution)
- Only repo maintainers can define cron jobs
- Commands run with agent's permissions
- `timeout` prevents runaway processes

## Phase 1 Non-Goals

- State delta / progress detection
- Stuck detection / escalation
- LLM judge
- Max rounds
- Multi-agent coordination logic
- Goal lifecycle slash commands

## MVP Test Scenario

**Setup:**
1. A repo with one failing test
2. Cron job: `disable_on_success = "cd /repo && npm test"`, schedule every 10 min
3. Agents have write access to the repo

**Expected behavior:**
1. Cron fires → `npm test` fails (exit 1) → message sent to thread
2. Agents discuss in thread, identify the bug, push a fix
3. Next cron fires → `npm test` passes (exit 0) → job auto-disables, no message sent
4. Done. Job stays disabled until human re-enables.

**Edge cases:**
- Process restarts between fires → state file preserves `thread_id` and `enabled` status
- Command hangs → killed after `timeout` seconds, treated as failure, message sent
- Human sets `enabled = true` in config → job re-activates (intentional reset)

---

# Phase 2: Full Goal Runner (Future Design)

When Phase 1 is proven, extend with richer goal semantics. Phase 2 will introduce a new `[[goals]]` config section; Phase 1 `[[cron.jobs]]` entries with `disable_on_success` remain valid and coexist — no migration required.

## Additional Capabilities

### State Delta Detection

Track progress between rounds using a `progress_check` command:

```toml
[[goals]]
id = "goal-001"
description = "All unit tests pass"
done_check = "cd /repo && npm test"
progress_check = "cd /repo && git log --oneline -5"
interval = "10m"
max_rounds = 10
stuck_threshold = 3
channel = "123456789012345678"
```

Delta signals: git commits, file changes, test result transitions, PR status, artifact existence.

### Stuck Detection & Escalation

| Signal | Judgment |
|--------|----------|
| Has state delta + eval fail | Progressing, continue |
| No state delta + eval fail | Stuck, increment counter |
| Counter >= stuck_threshold | Escalate to human |
| Round > max_rounds | Hard stop, escalate |

### Escalation Message

```
⚠️ Goal Stuck — Escalating
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Goal: All unit tests pass
Last successful delta: Round 5 — commit abc1234
Blocked reason: No state change for 3 consecutive rounds
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
1️⃣ Give hint, continue
2️⃣ Human fixes, agents verify
3️⃣ Adjust goal/eval command
4️⃣ Abandon goal
```

### Escalation Recovery Rules

| Human action | Effect |
|---|---|
| 1️⃣ Give hint | `stuck_counter` resets; `round` continues |
| 2️⃣ Human fixes | `stuck_counter` resets; `round` continues |
| 3️⃣ Adjust goal | Full reset (new goal) |
| 4️⃣ Abandon | Goal disabled |

Key: **`max_rounds` never resets** unless goal is redefined.

### LLM Judge (Tie-Breaker Only)

After `done_check` passes, optionally confirm intent alignment via LLM. Not involved every round.

### Round Message Format

```
🔐 Goal: All unit tests pass
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Round: 4 / 10
Status: ❌ Not achieved
Eval output: FAIL src/auth.test.ts — TypeError
Progress: ✅ Delta detected (commit abc1234)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

---

## Open Questions

1. **Multi-agent coordination** — How do agents avoid conflicting actions in escape room mode?
2. **Goal lifecycle commands** — Slash commands? Config reload?
3. **Observability** — How to surface goal progress history?
4. **Context window overflow** — Summarization strategy for long-running goals?

## References

- [Existing CronJob docs](./cronjob.md)
- [Discord thread for this design discussion](https://discord.com/channels/1491295327620169908/1504239931940409587)

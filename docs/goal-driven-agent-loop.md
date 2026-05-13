# Goal-Driven Agent Loop

Design spec for a goal-oriented execution mode where agents work autonomously until a defined objective is achieved.

## Problem

Today, agents respond to individual messages reactively. There is no mechanism to assign a persistent **goal** that agents must work toward across multiple rounds, self-organizing their approach without explicit step-by-step instructions.

## Non-Goals (MVP)

- Multi-agent goal contention / auto-claiming
- Complex scoring or partial-credit evaluation
- Long-term memory rewrite between rounds
- LLM judge involvement on every round
- UI/dashboard for goal management

## Concept: "Escape Room" Mode

The human sets a goal and a success condition. A CronJob periodically evaluates whether the goal is met. If not, it posts to the channel — agents must **self-organize** to figure out how to achieve it. They are not told what to do, only what the goal is and that it hasn't been met yet.

```
Human sets goal + eval command
         │
         ▼
┌──► CronJob fires (on interval)
│         │
│         ▼
│    Run done_check command
│         │
│    ┌────┴─────┐
│    │  Pass?   │
│    └────┬─────┘
│     No  │  Yes
│     │   │    │
│     ▼   │    ▼
│  Post to channel:    Goal achieved ✅
│  "Goal not met,      Disable CronJob
│   keep working"      Notify human
│         │
│         ▼
│  Agents discuss & act
│  (self-organized)
│         │
└─────────┘
     Next interval
```

## Goal Schema

```toml
[[goals]]
id = "goal-001"
description = "All unit tests pass on main branch"
done_check = "cd /repo && npm test"
progress_check = "cd /repo && git log --oneline -5"
interval = "10m"
max_rounds = 10
stuck_threshold = 3          # rounds without state delta → escalate
channel = "123456789012345678"
thread_id = ""               # optional: confine to existing thread
owner = ""                   # optional: assigned agent UID
enabled = true
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `id` | ✅ | — | Unique goal identifier |
| `description` | ✅ | — | Human-readable goal statement |
| `done_check` | ✅ | — | Shell command; exit 0 = goal achieved |
| `progress_check` | | — | Command to capture state snapshot for delta detection |
| `interval` | | `"10m"` | Evaluation interval (e.g. `5m`, `1h`) |
| `max_rounds` | | `10` | Hard cap on evaluation rounds |
| `stuck_threshold` | | `3` | Consecutive rounds without state delta before escalation |
| `channel` | ✅ | — | Target channel for agent communication |
| `thread_id` | | — | Confine discussion to a specific thread |
| `owner` | | — | Agent UID responsible for execution |
| `enabled` | | `true` | Toggle without removing config |

## Runner Loop State Machine

```
         ┌─────────┐
         │  IDLE   │ ◄── goal created, waiting for first interval
         └────┬────┘
              │ interval fires
              ▼
         ┌─────────┐
         │  EVAL   │ ◄── run done_check
         └────┬────┘
              │
       ┌──────┴──────┐
       │             │
   exit 0        exit != 0
       │             │
       ▼             ▼
  ┌────────┐   ┌──────────┐
  │  DONE  │   │ COMPARE  │ ◄── compute state delta
  └────────┘   └────┬─────┘
                    │
             ┌──────┴──────┐
             │             │
        has delta      no delta
             │             │
             ▼             ▼
       ┌──────────┐  ┌──────────┐
       │ CONTINUE │  │  STUCK   │ ◄── increment stuck_counter
       └──────────┘  └────┬─────┘
                          │
                   stuck_counter >= threshold?
                     │            │
                    Yes           No
                     │            │
                     ▼            ▼
               ┌───────────┐  ┌──────────┐
               │ ESCALATE  │  │ CONTINUE │
               └───────────┘  └──────────┘
```

### State Transitions

| From | Event | To | Action |
|------|-------|----|--------|
| IDLE | interval fires | EVAL | Run `done_check` |
| EVAL | exit 0 | DONE | Notify channel ✅, disable goal |
| EVAL | exit != 0 | COMPARE | Run `progress_check`, compute delta |
| COMPARE | has delta | CONTINUE | Reset stuck_counter, post round message |
| COMPARE | no delta | STUCK | Increment stuck_counter |
| STUCK | counter < threshold | CONTINUE | Post round message with warning |
| STUCK | counter >= threshold | ESCALATE | Notify human, pause goal |
| Any | round > max_rounds | ESCALATE | Hard stop, notify human |

## State Snapshot & Delta Detection

Each round captures a **state snapshot** via `progress_check`. Delta is computed by comparing current snapshot to previous round's snapshot.

### Supported Delta Signals (MVP)

| Signal | How to detect |
|--------|---------------|
| New commits | `git log --oneline` diff |
| File changes | `git diff --stat` |
| Test result change | Test output diff (pass/fail count) |
| PR/Issue status | `gh pr view` / `gh issue view` |
| Artifact existence | `ls` / `stat` on expected path |

If no `progress_check` is defined, delta detection falls back to comparing `done_check` stdout/stderr between rounds.

## Round Message Format

Posted to channel/thread each round when goal is not yet achieved:

```
🔐 Goal: All unit tests pass on main branch
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Round: 4 / 10
Status: ❌ Not achieved
Eval output:
  FAIL src/auth.test.ts — TypeError: undefined is not a function
  Tests: 12 passed, 1 failed
Progress: ✅ Delta detected (new commit abc1234)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
法師們，繼續想辦法。
```

When stuck (no delta):

```
🔐 Goal: All unit tests pass on main branch
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Round: 7 / 10
Status: ❌ Not achieved
Eval output:
  FAIL src/auth.test.ts — TypeError: undefined is not a function
Progress: ⚠️ No state delta (2 / 3 rounds until escalation)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
法師們，繼續想辦法。
```

## Escalation Payload

When stuck_threshold is reached or max_rounds exceeded:

```
⚠️ Goal Stuck — Escalating
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Goal: All unit tests pass on main branch
Last successful delta: Round 5 — fixed auth.test.ts (commit abc1234)
Blocked reason: No state change for 3 consecutive rounds
Current eval output:
  FAIL src/auth.test.ts — TypeError: undefined is not a function
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
需要主人決策：
1️⃣ 給提示讓法師繼續
2️⃣ 主人自己修，修完再讓法師 verify
3️⃣ 調整 goal 或 eval command
4️⃣ 放棄此 goal
```

## Done Confirmation (Optional LLM Judge)

When `done_check` passes (exit 0), an optional LLM judge can confirm intent alignment:

```
done_check passes
       │
       ▼
  LLM Judge: "Does the current state satisfy the goal description?"
       │
  ┌────┴────┐
  │         │
confirm   reject + reason
  │         │
  ▼         ▼
DONE     CONTINUE (post rejection reason to channel)
```

This is a **tie-breaker only** — not involved in every round. Only fires after Layer 1 (deterministic check) passes.

## Integration with Existing CronJob

This feature extends the existing `[[cron.jobs]]` system. Implementation options:

1. **New config section** `[[goals]]` — separate from `[[cron.jobs]]`, dedicated runner logic
2. **Extension of cron** — add `goal_mode = true` fields to existing cron entries

Recommended: **Option 1** — separate section. Goal semantics (state tracking, delta detection, escalation) are fundamentally different from simple scheduled messages.

## MVP Test Scenario

**Setup:**
1. A repo with one failing test
2. Goal: `done_check = "npm test"` with exit 0 = success
3. Agent has write access to the repo

**Expected behavior:**
1. CronJob fires → runs `npm test` → fails → posts round message
2. Agents discuss in thread, identify the bug, push a fix
3. Next CronJob fires → runs `npm test` → passes → posts ✅ Done
4. Goal disabled

**Stuck scenario:**
1. Agents cannot figure out the fix
2. 3 consecutive rounds with no new commits
3. Escalation message posted, goal paused

## Security: Shell Execution

`done_check` and `progress_check` execute arbitrary shell commands. Mitigation strategy:

| Phase | Mitigation |
|-------|-----------|
| MVP | Trust config source — only repo maintainers can define goals. Document that commands run with agent's permissions. |
| v2 | Allowed command whitelist + read-only mode for `progress_check` |
| v3 | Container isolation — run eval commands in ephemeral sandbox with no network/write access to host |

MVP explicitly does NOT sandbox. This is acceptable because config is maintainer-controlled (same trust model as existing `[[cron.jobs]]`).

## Persistence

Goal state **must be persisted** to survive process restarts. Without persistence, `max_rounds` and `stuck_threshold` safety valves can be bypassed by restarts.

Persisted state per goal:

```json
{
  "goal_id": "goal-001",
  "round": 4,
  "stuck_counter": 1,
  "last_snapshot": "abc1234...",
  "last_eval_output": "FAIL src/auth.test.ts...",
  "status": "active",
  "history": [
    { "round": 1, "delta": true, "timestamp": "..." },
    { "round": 2, "delta": true, "timestamp": "..." },
    { "round": 3, "delta": false, "timestamp": "..." }
  ]
}
```

MVP storage: local JSON file (`goals-state.json`). Future: DB or object store.

## Escalation Recovery Rules

When the human responds to an escalation:

| Human action | Effect on counters |
|---|---|
| 1️⃣ Give hint, continue | `stuck_counter` resets to 0; `round` continues (does NOT reset) |
| 2️⃣ Human fixes, agents verify | `stuck_counter` resets to 0; `round` continues |
| 3️⃣ Adjust goal/eval | `stuck_counter` resets to 0; `round` resets to 0 (new goal effectively) |
| 4️⃣ Abandon goal | `status` = `abandoned`, goal disabled |

Key principle: **`max_rounds` never resets** unless the goal itself is redefined (option 3). This prevents infinite loops even with repeated escalations.

## Open Questions

1. **Thread vs channel** — Should each goal auto-create a dedicated thread, or reuse an existing one?
2. **Multi-agent coordination** — In escape room mode, how do agents avoid conflicting actions? First-come-first-serve? Or coordinator (超渡) assigns sub-tasks?
3. **Goal lifecycle commands** — How does the human create/pause/cancel goals? Slash commands? Config file reload?
4. **Observability** — How to surface goal progress history (rounds, deltas, escalations)?
5. **Context window overflow** — Long-running goals accumulate thread history. Should each round message include a condensed summary of prior rounds to prevent context overflow? Or implement a sliding window / summarization step?

## References

- [Existing CronJob docs](./cronjob.md)
- [Discord thread for this design discussion](https://discord.com/channels/1491295927620169908/1504239931940409587)

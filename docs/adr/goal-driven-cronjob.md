# ADR: Goal-Driven CronJob (disable_on_success)

- **Status:** Proposed
- **Date:** 2026-05-13
- **Author:** @chaodu-agent
- **Related:** [Basic CronJob ADR](./basic-cronjob.md), [CronJob Docs](../cronjob.md)

---

## 1. User Story & Requirements

As an OpenAB operator, I want to define a **goal** that agents must achieve, where a CronJob periodically checks if the goal is met and keeps prompting agents until it is — so that I can assign persistent objectives without manually following up.

As a team lead, I want agents to self-organize ("escape room" mode) — I tell them the goal, not the steps.

Requirements:
- Extend existing `[[cron.jobs]]` with a `disable_on_success` field
- Before sending the scheduled message, run the specified command
- If command exits 0 → goal achieved, auto-disable the job, do NOT send message
- If command exits non-zero → goal not met, send message as normal (agents continue working)
- Auto-disable state must persist across restarts
- Human can re-enable a completed goal by bumping `generation`
- All communication stays in a single stable thread

---

## 2. Context & Decision Drivers

### The "Escape Room" Pattern

Traditional agent interaction is reactive: human sends message, agent responds. This ADR introduces **goal-driven** interaction: human sets an objective, agents work autonomously across multiple rounds until the objective is met.

The key insight: we don't need a complex goal orchestrator for Phase 1. The existing CronJob scheduler already provides periodic execution — we just need to add a "stop condition."

### Why Extend CronJob (Not a New System)

We considered two approaches:

| Approach | Pros | Cons |
|----------|------|------|
| New `[[goals]]` config section | Clean separation, dedicated semantics | New scheduler, new state machine, large MVP |
| Extend `[[cron.jobs]]` | Minimal change, reuses existing infra | Slightly overloaded config section |

**Decision: Extend `[[cron.jobs]]`** — Phase 1 is literally "cron + exit check + auto-disable." The existing scheduler, channel routing, and thread handling all apply. A full goal runner with state delta detection and escalation is deferred to Phase 2.

### Design Principle: Smallest Useful Increment

> "Don't build a goal orchestrator when a conditional cron job will do."

Phase 1 proves the concept. Phase 2 adds sophistication only after validation.

---

## 3. Design

### Configuration

```toml
[[cron.jobs]]
id = "unit-tests-pass"                            # required for disable_on_success jobs
schedule = "*/10 * * * *"
channel = "123456789012345678"
thread_id = ""                                    # auto-created on first fire if empty
message = "Goal not met: all unit tests must pass. Please continue working."
disable_on_success = "npm test"                   # command to evaluate goal
disable_on_success_timeout_secs = 60              # command timeout
disable_on_success_working_dir = "/repo"          # working directory
generation = 1                                    # bump to re-enable after auto-disable
```

### New Fields

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `id` | ✅ (when `disable_on_success` set) | — | Stable unique identifier for state persistence |
| `disable_on_success` | | — | Shell command; exit 0 = goal achieved, auto-disable |
| `disable_on_success_timeout_secs` | | `60` | Max seconds before command is killed |
| `disable_on_success_working_dir` | | — | Working directory for command execution |
| `generation` | | `1` | Bump to re-enable an auto-disabled job |

### Execution Flow

```
CronJob schedule fires
         │
         ▼
  Is job auto-disabled AND config generation == persisted generation?
         │
    ┌────┴────┐
   Yes        No
    │          │
    ▼          ▼
  Skip    Run disable_on_success command
  (done)       │
          ┌────┴────┐
          │ exit 0? │
          └────┬────┘
          Yes  │  No / Timeout
           │   │    │
           ▼   │    ▼
     Auto-disable   Send message
     job, persist   to channel/thread
     state          (agents keep working)
```

### State Persistence

Persisted in `cron-state.json`, keyed by job `id`:

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

Loaded on startup, written on state change.

### Re-enable Logic

```
config.generation > persisted.generation?
    │
   Yes → Clear auto_disabled state, job runs again
   No  → Job stays disabled
```

Human bumps `generation = 2` in config → job reactivates. No ambiguity, no conflict with `enabled` field.

### Thread Lifecycle

| Scenario | Behavior |
|----------|----------|
| `thread_id` provided in config | Use that thread for all fires |
| `thread_id` empty | Auto-create thread on first fire, persist in state |

All messages go to the **same thread** — agents need conversation history as context across rounds.

### Security

| Concern | Mitigation |
|---------|-----------|
| Arbitrary shell execution | Trust config source (same as existing cron). Only maintainers edit config. |
| Runaway commands | `disable_on_success_timeout_secs` kills long-running processes |
| Command injection | Config is static TOML, not user-input at runtime |

Future phases may add container isolation or command whitelists.

---

## 4. Implementation Plan

### Phase 1 (This ADR)

1. Parse new fields from `[[cron.jobs]]` config
2. On cron fire, if `disable_on_success` is set:
   - Check persisted state (generation match → skip if auto-disabled)
   - Execute command with timeout and working_dir
   - exit 0 → persist auto-disabled state, skip message
   - exit != 0 / timeout → send message as normal
3. Thread auto-creation: if `thread_id` empty, create thread on first fire, persist
4. State file: read/write `cron-state.json`

### Phase 2 (Future — Not This ADR)

Introduce `[[goals]]` config section with:
- `progress_check` — state delta detection between rounds
- `stuck_threshold` — escalate after N rounds without progress
- `max_rounds` — hard cap
- LLM judge — tie-breaker after command passes
- Escalation messages with decision options
- Round counter and progress reporting

Phase 1 `[[cron.jobs]]` entries with `disable_on_success` remain valid and coexist with Phase 2 `[[goals]]` — no migration required.

---

## 5. Test Scenarios

### Happy Path

1. Repo has one failing test
2. Cron fires every 10 min with `disable_on_success = "npm test"`
3. `npm test` fails → message sent → agents discuss and fix
4. Next fire → `npm test` passes → job auto-disables, no message

### Restart Resilience

1. Job is auto-disabled (goal achieved)
2. Process restarts
3. State loaded from `cron-state.json` → job stays disabled

### Re-enable

1. Job is auto-disabled (`generation: 1` in state)
2. Human bumps config to `generation = 2`
3. Next fire → generation mismatch → clear auto-disable → run command again

### Timeout

1. `disable_on_success` command hangs
2. After `disable_on_success_timeout_secs` → killed
3. Treated as failure → message sent

---

## 6. Open Questions

1. **Multi-agent coordination** — How do agents avoid conflicting actions when self-organizing?
2. **Observability** — Should we log command output / exit codes for debugging?
3. **Context overflow** — Long-running goals accumulate thread history; summarization strategy TBD
4. **Notification on success** — Should auto-disable post a "✅ Goal achieved" message, or silently stop?

---

## 7. References

- [Basic CronJob ADR](./basic-cronjob.md)
- [CronJob Docs](../cronjob.md)
- [Design Discussion (Discord)](https://discord.com/channels/1491295327620169908/1504239931940409587)

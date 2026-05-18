# Reference Architecture: CI Observability via Discord

> **This doc is meant to be used with Kiro or any coding CLI.** Prompt your AI agent with something like:
>
> ```
> per https://github.com/openabdev/openab/blob/main/docs/refarch/ci-discord-notify.md set up CI notifications to my Discord channel
> ```
>
> and it will guide you through the full setup.

Send GitHub Actions CI results (pass/fail) to a Discord channel or thread via webhook, with clickable links, duration, and user mentions.

## Problem

When CI runs in GitHub Actions, the only way to know the result is to check the GitHub UI or wait for an email. For teams collaborating in Discord, this creates friction:

- **No visibility** — CI failures go unnoticed until someone manually checks GitHub
- **Slow feedback loop** — contributors wait without knowing their PR is broken
- **Context switching** — developers must leave Discord to check CI status
- **No accountability** — nobody gets pinged when CI breaks

## What We Want

- CI finishes (pass or fail) → automatically POST result to a specific Discord channel/thread
- Commit message is a **clickable link** pointing to the PR or commit
- Show who committed, how long CI took, and which step failed
- Mention a specific user so they get pinged
- Route notifications to the correct thread based on the PR description
- One reusable workflow that any CI job can call

## Challenges

| Challenge | Why it's hard |
|-----------|---------------|
| Notify regardless of outcome | GitHub Actions skips downstream jobs when upstream fails — need `if: always()` |
| Clickable links in Discord | Webhook `content` doesn't support markdown links — bare URLs auto-link |
| Newlines in message | Shell variables + `jq --arg` lose newlines — must write to file and use `jq --rawfile` |
| Route to the right thread | Different PRs need notifications in different threads — need dynamic extraction |
| Don't repeat yourself | Multiple CI workflows need the same notification logic — need reusable workflow |
| Keep secrets safe | Webhook URL contains a token — must never appear in workflow files or logs |

## Two Approaches

### Approach 1: Polling Mode (Cronjob)

OpenAB has a built-in cron scheduler. You can schedule the agent to periodically check CI status and fix failures:

```
@bot can you schedule a cronjob for yourself to this thread and remind yourself to
"check https://github.com/owner/repo/actions and fix them if required" every 10min?
```

This creates a `[[cron.jobs]]` entry:

```toml
[[cron.jobs]]
schedule = "*/10 * * * *"
channel = "123456789012345678"
thread_id = "1505664791719710810"
message = "check https://github.com/owner/repo/actions and fix them if required"
```

**Pros:** Holistic view — checks everything on your plate (all workflows, all branches, all repos). Agent can auto-fix issues. No webhook configuration needed.

**Cons:** Up to N-minute delay, unnecessary API calls when nothing changed, burns compute on polling.

### Approach 2: Notification Mode (Webhook Push) ← This Doc

CI pushes results to Discord the moment it finishes — zero delay, zero wasted calls. But it only tells you about **this single CI run**.

```
GitHub Actions ──finish──► HTTP POST ──► Discord thread
                              (webhook)
```

**Pros:** Instant notification, no polling cost, precise metadata (duration, failed step, commit info) for the specific run.

**Cons:** Narrow scope — only reports on the workflow that triggered it. Can't see the big picture. Can't auto-fix (notification only). Requires webhook setup.

### When to Use Which

| | Polling (Cronjob) | Notification (Webhook) |
|---|---|---|
| **Scope** | Everything on your plate — all workflows, branches, repos | Single CI run only |
| **Latency** | Up to N minutes | Instant (on completion) |
| **Auto-fix** | ✅ Agent can push fixes | ❌ Notification only |
| **Setup** | Just tell the bot | Webhook + secrets + workflow changes |
| **Cost** | Burns compute even when idle | Zero cost when nothing runs |
| **Metadata** | Whatever the agent can scrape | Precise: duration, failed step, commit SHA |
| **Best for** | "Keep my CI green across all repos" | "Tell me the moment this PR breaks" |

| Scenario | Recommended |
|----------|-------------|
| "Tell me when CI breaks" | Notification mode (this doc) |
| "Check CI and fix it automatically" | Polling mode (cronjob) |
| Both — notify immediately + auto-fix | Combine: webhook notifies, cronjob retries fixes |

---

## Solution

A **reusable workflow** (`notify-discord.yml`) that any CI workflow calls as its final job. It posts all CI metadata in the Discord `content` field (with real newlines) and mentions the target bot/user — routing to the correct thread based on the PR description.

## Architecture

```
+-- GitHub Actions ----------------------------------------+
|                                                          |
|  +-- ci.yml ------------------------------------------+  |
|  |                                                    |  |
|  |  [check] ──► cargo fmt / clippy / test             |  |
|  |     │                                              |  |
|  |     │ outputs: status, duration, commit_msg,       |  |
|  |     │          commit_author, commit_sha           |  |
|  |     ▼                                              |  |
|  |  [notify] (if: always())                           |  |
|  |     │  calls ──► notify-discord.yml (reusable)     |  |
|  |     │                                              |  |
|  +-----|----------------------------------------------+  |
|        │                                                 |
|        │  inputs: status, commit_msg, pr_body, ...       |
|        │  secrets: DISCORD_WEBHOOK_URL                    |
|        │  vars: DISCORD_THREAD_ID, DISCORD_MENTION_UID   |
|        │                                                 |
+--------|─────────────────────────────────────────────────+
         │
         │ HTTP POST (webhook + ?thread_id=xxx)
         ▼
+-- Discord -----------------------------------------------+
|                                                          |
|  #channel or thread                                      |
|  ┌─────────────────────────────────────────────────┐     |
|  │ ✅ **CI success** — `repo@main`                 │     |
|  │ 👤 author — `feat: add new provider`            │     |
|  │ ⏱️ 3m42s                                        │     |
|  │ https://github.com/.../actions/runs/123         │     |
|  │ @bot-mention                                    │     |
|  └─────────────────────────────────────────────────┘     |
|                                                          |
+----------------------------------------------------------+
```

## Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| Reusable workflow (`workflow_call`) | Any CI workflow can call it; single source of truth |
| `if: always()` on notify job | Fires on success, failure, and cancellation |
| All info in `content` (not embed) | Bots can read `content` directly; embeds are invisible to most bot frameworks |
| Thread ID from PR body | Dynamic routing — each PR notifies its own thread |
| Fallback to repo variable | Push-to-main events still get notified somewhere |
| Write message to file + `jq --rawfile` | Guarantees real newlines in JSON payload (shell variables lose trailing newlines) |
| `allowed_mentions: {parse: ["users"]}` | Required for webhook `<@UID>` mentions to actually ping |

## Setup

### 1. Create a Discord Webhook

Server Settings → Integrations → Webhooks → New Webhook → Copy URL.

### 2. Configure Repository Secrets & Variables

| Type | Name | Value |
|------|------|-------|
| **Secret** | `DISCORD_WEBHOOK_URL` | The webhook URL (contains token — keep secret) |
| **Variable** | `DISCORD_THREAD_ID` | Default thread ID for fallback notifications |
| **Variable** | `DISCORD_MENTION_USER_ID` | Discord user ID to mention (e.g. `1234567890`) |

Set via CLI:

```bash
gh secret set DISCORD_WEBHOOK_URL --repo <owner>/<repo>
gh variable set DISCORD_THREAD_ID --repo <owner>/<repo> --body "<thread_id>"
gh variable set DISCORD_MENTION_USER_ID --repo <owner>/<repo> --body "<user_id>"
```

### 3. Create the Reusable Workflow

`.github/workflows/notify-discord.yml`:

```yaml
name: Discord Notify

on:
  workflow_call:
    inputs:
      status:
        required: true
        type: string
      failed_step:
        required: false
        type: string
      duration:
        required: false
        type: string
      commit_msg:
        required: false
        type: string
      commit_author:
        required: false
        type: string
      commit_sha:
        required: false
        type: string
      pr_body:
        required: false
        type: string
    secrets:
      DISCORD_WEBHOOK_URL:
        required: true

jobs:
  notify:
    runs-on: ubuntu-latest
    environment: discord-notify
    steps:
      - name: Send Discord notification
        env:
          WEBHOOK_URL: ${{ secrets.DISCORD_WEBHOOK_URL }}
          DEFAULT_THREAD_ID: ${{ vars.DISCORD_THREAD_ID }}
          MENTION_USER_ID: ${{ vars.DISCORD_MENTION_USER_ID }}
          STATUS: ${{ inputs.status }}
          FAILED_STEP: ${{ inputs.failed_step }}
          DURATION: ${{ inputs.duration }}
          COMMIT_MSG: ${{ inputs.commit_msg }}
          COMMIT_AUTHOR: ${{ inputs.commit_author }}
          COMMIT_SHA: ${{ inputs.commit_sha }}
          PR_BODY: ${{ inputs.pr_body }}
          RUN_URL: ${{ github.server_url }}/${{ github.repository }}/actions/runs/${{ github.run_id }}
          REPO: ${{ github.repository }}
          REF: ${{ github.ref_name }}
          PR: ${{ github.event.pull_request.number }}
          SERVER_URL: ${{ github.server_url }}
        run: |
          # Extract Thread ID from PR body, fallback to variable
          THREAD_ID=""
          if [ -n "$PR_BODY" ]; then
            THREAD_ID=$(echo "$PR_BODY" | grep -ioP '^Thread:\s*\K[0-9]+' | head -1)
          fi
          [ -z "$THREAD_ID" ] && THREAD_ID="$DEFAULT_THREAD_ID"

          if [ "$STATUS" = "success" ]; then
            EMOJI="✅"
          else
            EMOJI="❌"
          fi

          # Build message into a temp file for proper newlines
          {
            printf '%s **CI %s** — `%s@%s`' "$EMOJI" "$STATUS" "$REPO" "$REF"
            [ -n "$PR" ] && printf ' | PR #%s' "$PR"
            echo ""
            [ -n "$COMMIT_AUTHOR" ] && printf '👤 %s' "$COMMIT_AUTHOR"
            [ -n "$COMMIT_MSG" ] && printf ' — `%s`' "$COMMIT_MSG"
            [ -n "$COMMIT_AUTHOR" ] && echo ""
            [ -n "$DURATION" ] && echo "⏱️ ${DURATION}"
            [ "$STATUS" != "success" ] && [ -n "$FAILED_STEP" ] && echo "💥 Failed at: **${FAILED_STEP}**"
            echo "$RUN_URL"
            [ -n "$MENTION_USER_ID" ] && echo "<@${MENTION_USER_ID}>"
          } > /tmp/msg.txt

          # Use jq to read file content and build safe JSON
          PAYLOAD=$(jq -n --rawfile msg /tmp/msg.txt \
            '{content: $msg, allowed_mentions: {parse: ["users"]}}')

          URL="${WEBHOOK_URL}"
          [ -n "$THREAD_ID" ] && URL="${URL}?thread_id=${THREAD_ID}"

          curl -sf -X POST "$URL" \
            -H "Content-Type: application/json" \
            -d "$PAYLOAD"
```

### 4. Wire Into Your CI Workflow

Add a `notify` job at the end of any workflow:

```yaml
jobs:
  check:
    runs-on: ubuntu-latest
    outputs:
      duration: ${{ steps.duration.outputs.value }}
      commit_msg: ${{ steps.meta.outputs.commit_msg }}
      commit_author: ${{ steps.meta.outputs.commit_author }}
      failed_step: ${{ steps.meta.outputs.failed_step }}
    steps:
      - name: Record start time
        id: start
        run: echo "ts=$(date +%s)" >> "$GITHUB_OUTPUT"

      # ... your build/test steps (give each an id) ...

      - name: Collect metadata
        id: meta
        if: always()
        run: |
          echo "commit_msg=$(git log -1 --pretty=%s)" >> "$GITHUB_OUTPUT"
          echo "commit_author=$(git log -1 --pretty=%an)" >> "$GITHUB_OUTPUT"
          # Detect which step failed
          FAILED=""
          # if [ "${{ steps.test.outcome }}" = "failure" ]; then FAILED="Tests"; fi
          echo "failed_step=${FAILED}" >> "$GITHUB_OUTPUT"

      - name: Calculate duration
        id: duration
        if: always()
        run: |
          ELAPSED=$(( $(date +%s) - ${{ steps.start.outputs.ts }} ))
          echo "value=$((ELAPSED/60))m$((ELAPSED%60))s" >> "$GITHUB_OUTPUT"

  notify:
    needs: [check]
    if: always()
    uses: ./.github/workflows/notify-discord.yml
    with:
      status: ${{ needs.check.result }}
      failed_step: ${{ needs.check.outputs.failed_step }}
      duration: ${{ needs.check.outputs.duration }}
      commit_msg: ${{ needs.check.outputs.commit_msg }}
      commit_author: ${{ needs.check.outputs.commit_author }}
      commit_sha: ${{ github.event.pull_request.head.sha || github.sha }}
      pr_body: ${{ github.event.pull_request.body }}
    secrets:
      DISCORD_WEBHOOK_URL: ${{ secrets.DISCORD_WEBHOOK_URL }}
```

### 5. Dynamic Thread Routing via PR Description

Add a `Thread:` line anywhere in your PR description:

```
Thread: 1505664791719710810
```

The workflow extracts the first match and posts to that thread. If absent, it falls back to `DISCORD_THREAD_ID` variable.

## Gotchas

| Issue | Solution |
|-------|----------|
| `content` field doesn't support markdown links | URLs are auto-linked by Discord; use bare URLs for clickability |
| `\n` in shell variables becomes literal `\\n` in JSON | Write message to a file, use `jq --rawfile` to read it |
| Bots can't read embed content | Put all info in `content` field, not embeds |
| Webhook URL contains a token | Always store as a **secret**, never in workflow files or docs |
| `if: always()` required on notify job | Otherwise it's skipped when upstream jobs fail |
| Mention doesn't ping | Add `allowed_mentions: {parse: ["users"]}` to payload |
| Mention alone doesn't trigger bot | Include message text alongside `<@UID>` in content |

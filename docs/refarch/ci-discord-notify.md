# Reference Architecture: CI-to-Discord Notification

> **This doc is meant to be used with Kiro or any coding CLI.** Prompt your AI agent with something like:
>
> ```
> per https://github.com/openabdev/openab/blob/main/docs/refarch/ci-discord-notify.md set up CI notifications to my Discord channel
> ```
>
> and it will guide you through the full setup.

Send GitHub Actions CI results (pass/fail) to a Discord channel or thread via webhook, with clickable links, duration, and bot mentions.

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
|  │ ✅ feat: add new provider        ← clickable    │     |
|  │ ──────────────────────────────────────────────  │     |
|  │ ✅ CI success — repo@main                       │     |
|  │ 👤 author                                       │     |
|  │ ⏱️ 3m42s                                        │     |
|  │ View Run                          ← clickable   │     |
|  └─────────────────────────────────────────────────┘     |
|  @bot-mention                                            |
|                                                          |
+----------------------------------------------------------+
```

## Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| Reusable workflow (`workflow_call`) | Any CI workflow can call it; single source of truth |
| `if: always()` on notify job | Fires on success, failure, and cancellation |
| Discord embed (not plain content) | Supports clickable title, colored sidebar, markdown in description |
| Thread ID from PR body | Dynamic routing — each PR notifies its own thread |
| Fallback to repo variable | Push-to-main events still get notified somewhere |
| `printf` for newlines | `jq --arg` preserves real `\n` from printf output |

## Setup

### 1. Create a Discord Webhook

Server Settings → Integrations → Webhooks → New Webhook → Copy URL.

### 2. Configure Repository Secrets & Variables

| Type | Name | Value |
|------|------|-------|
| **Secret** | `DISCORD_WEBHOOK_URL` | The webhook URL (contains token — keep secret) |
| **Variable** | `DISCORD_THREAD_ID` | Default thread ID for fallback notifications |
| **Variable** | `DISCORD_MENTION_USER_ID` | Discord user/bot ID to mention (e.g. `1234567890`) |

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
            COLOR=3066993; EMOJI="✅"
          else
            COLOR=15158332; EMOJI="❌"
          fi

          # Embed title = commit msg (clickable link to PR or commit)
          TITLE="${COMMIT_MSG:-CI ${STATUS}}"
          if [ -n "$PR" ]; then
            TITLE_URL="${SERVER_URL}/${REPO}/pull/${PR}"
          elif [ -n "$COMMIT_SHA" ]; then
            TITLE_URL="${SERVER_URL}/${REPO}/commit/${COMMIT_SHA}"
          else
            TITLE_URL="${RUN_URL}"
          fi

          # Build description using printf for real newlines
          DESC="${EMOJI} **CI ${STATUS}** — \`${REPO}@${REF}\`"
          [ -n "$PR" ] && DESC="${DESC} | PR #${PR}"
          [ -n "$COMMIT_AUTHOR" ] && DESC=$(printf "%s\n👤 %s" "$DESC" "$COMMIT_AUTHOR")
          [ -n "$DURATION" ] && DESC=$(printf "%s\n⏱️ %s" "$DESC" "$DURATION")
          [ "$STATUS" != "success" ] && [ -n "$FAILED_STEP" ] && \
            DESC=$(printf "%s\n💥 Failed at: **%s**" "$DESC" "$FAILED_STEP")
          DESC=$(printf "%s\n[View Run](%s)" "$DESC" "$RUN_URL")

          # Build JSON payload
          CONTENT=""
          [ -n "$MENTION_USER_ID" ] && CONTENT="<@${MENTION_USER_ID}>"

          PAYLOAD=$(jq -n \
            --arg content "$CONTENT" \
            --arg title "$TITLE" \
            --arg url "$TITLE_URL" \
            --arg desc "$DESC" \
            --argjson color "$COLOR" \
            '{content: $content, embeds: [{title: $title, url: $url, description: $desc, color: $color}]}')

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
| `content` field doesn't support markdown links | Use `embeds` with `title`/`url` for clickable links |
| `\n` in `jq --arg` becomes literal `\\n` | Use `printf` to produce real newlines before passing to jq |
| Duplicate YAML keys silently break workflows | Validate with `actionlint` or check Actions run errors |
| Webhook URL contains a token | Always store as a **secret**, never in workflow files or docs |
| `if: always()` required on notify job | Otherwise it's skipped when upstream jobs fail |
| Mention requires numeric Discord user ID | Use `<@USER_ID>` format in `content` (not in embed) |

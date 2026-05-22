# Google Antigravity CLI (agy)

OpenAB supports [Google Antigravity CLI](https://antigravity.google/) via the `agy-acp` adapter — a thin Rust binary that translates ACP JSON-RPC into `agy -p` invocations.

## How It Works

```
openab ──ACP JSON-RPC──► agy-acp ──spawns──► agy --dangerously-skip-permissions -p "prompt"
                                              agy --continue -p "follow-up"
```

- First prompt in a session: `agy -p "text"`
- Subsequent prompts: `agy --continue -p "text"` (resumes most recent conversation)
- Tool permissions are auto-approved via `--dangerously-skip-permissions`

## Configuration

```toml
[agent]
command = "agy-acp"
args = []
working_dir = "/home/agent"
```

Or with the Docker image:

```toml
[agent]
command = "/usr/local/bin/agy-acp"
args = []
working_dir = "/home/agent"
```

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `AGY_WORKING_DIR` | Working directory for agy invocations | `/tmp` |

## Docker

```bash
docker build -f Dockerfile.antigravity -t openab-antigravity .
```

## Authentication

Antigravity CLI uses Google Sign-In (OAuth). Authenticate inside the container:

```bash
kubectl exec -it deployment/openab-antigravity -- agy auth
```

Complete the device flow in your browser. Auth tokens persist in the PVC at `~/.gemini/`.

## Helm

```yaml
agents:
  antigravity:
    discord:
      botToken: "${DISCORD_BOT_TOKEN}"
      allowedChannels: ["123456789"]
    agent:
      command: "agy-acp"
      args: []
      workingDir: "/home/agent"
      env:
        AGY_WORKING_DIR: "/home/agent"
    image:
      repository: ghcr.io/openabdev/openab-antigravity
      tag: "latest"
```

## Limitations

- **No streaming**: `agy -p` returns the full response at once; the adapter sends it as a single `agent_message_chunk` notification.
- **Cancel is a no-op**: `agy -p` runs to completion; `session/cancel` acknowledges but cannot interrupt.
- **Session continuity uses `--continue`**: This resumes the *most recent* agy conversation, which works for single-user-per-pod deployments but may conflict if multiple sessions run concurrently in the same container.

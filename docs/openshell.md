# OpenShell

Run OAB inside an [NVIDIA OpenShell](https://github.com/NVIDIA/OpenShell) sandbox for isolated, policy-enforced execution with credential injection.

## Prerequisites

- Docker running on the host (user must be in the `docker` group)
- [OpenShell CLI](https://github.com/NVIDIA/OpenShell#install) installed

```bash
curl -LsSf https://raw.githubusercontent.com/NVIDIA/OpenShell/main/install.sh | sh
```

> **Note:** The installer starts `openshell-gateway` as a systemd user service. If the gateway fails with "failed to query Docker daemon version", add your user to the `docker` group and restart the session:
> ```bash
> sudo usermod -aG docker $USER
> # Log out and back in (or: loginctl terminate-user $USER)
> ```

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│  Host (Linux with Docker)                                           │
│                                                                     │
│  ┌───────────────────────────────────────────────────────────────┐  │
│  │  OpenShell Gateway (systemd user service :17670)              │  │
│  │  • manages sandbox lifecycle                                  │  │
│  │  • enforces network policy (default-deny egress)              │  │
│  │  • injects provider credentials into sandbox env              │  │
│  └──────────────────────────┬────────────────────────────────────┘  │
│                             │ creates & policies                     │
│                             ▼                                        │
│  ┌───────────────────────────────────────────────────────────────┐  │
│  │  Docker Container (sandbox: "oab")                            │  │
│  │                                                               │  │
│  │  /sandbox/                                                    │  │
│  │  ├── openab              ← ACP broker binary                  │  │
│  │  ├── openab-agent        ← native Rust coding agent           │  │
│  │  ├── config.toml         ← bot token + agent config           │  │
│  │  └── .openab/agent/auth.json  ← codex OAuth token             │  │
│  │                                                               │  │
│  │  openab run ──stdio JSON-RPC──► openab-agent                  │  │
│  │       │                              │                        │  │
│  │       │ Discord WS                   │ ChatGPT API            │  │
│  └───────┼──────────────────────────────┼────────────────────────┘  │
│           │                              │                           │
│  ┌────────┼──────────────────────────────┼────────────────────┐     │
│  │ Network Policy (egress allowlist)     │                    │     │
│  │  ✓ discord.com:443                    │                    │     │
│  │  ✓ gateway.discord.gg:443            │                    │     │
│  │  ✓ cdn.discordapp.com:443            │                    │     │
│  │  ✓ chatgpt.com:443  ◄────────────────┘                    │     │
│  │  ✓ auth0.openai.com:443                                   │     │
│  │  ✗ everything else DENIED                                  │     │
│  └────────┼───────────────────────────────────────────────────┘     │
└───────────┼─────────────────────────────────────────────────────────┘
            │
            ▼
┌──────────────────┐         ┌──────────────────┐
│  Discord API     │         │  ChatGPT API     │
│  (bot gateway)   │         │  (chatgpt.com)   │
└──────────────────┘         └──────────────────┘
```

## Quick Start (Local Docker)

All commands run **on the host** unless prefixed with `sandbox$`.

### 1. Create credential provider

```bash
export DISCORD_BOT_TOKEN="your-token"

openshell provider create --name discord --type generic \
  --credential "DISCORD_BOT_TOKEN=${DISCORD_BOT_TOKEN}"
```

### 2. Create sandbox

```bash
openshell sandbox create --name oab \
  --provider discord \
  -- bash
```

At this point you are **inside the sandbox** (prompt changes). To return to the host, type `exit`. To reconnect later: `openshell sandbox connect oab`.

### 3. Install OAB + Native Agent (from host)

The sandbox has **default-deny egress**, so download binaries on the host and copy them in:

```bash
# On the host (separate terminal)
TAG=$(curl -sI https://github.com/openabdev/openab/releases/latest | grep -i location | sed 's|.*/||' | tr -d '\r')

# Download OAB
curl -LO "https://github.com/openabdev/openab/releases/download/${TAG}/${TAG}-linux-x64.tar.gz"
tar xzf ${TAG}-linux-x64.tar.gz

# Extract openab-agent from the native image
docker pull ghcr.io/openabdev/openab-native:beta
CID=$(docker create ghcr.io/openabdev/openab-native:beta)
docker cp $CID:/usr/local/bin/openab-agent ./openab-agent
docker rm $CID

# Copy into sandbox
CONTAINER=$(docker ps --filter name=openshell-oab -q)
docker cp openab $CONTAINER:/sandbox/
docker cp openab-agent $CONTAINER:/sandbox/
docker exec $CONTAINER chmod +x /sandbox/openab /sandbox/openab-agent
```

### 4. Authenticate openab-agent (codex OAuth — headless)

```bash
# On the host
docker exec -it $CONTAINER /sandbox/openab-agent auth codex-oauth --no-browser
```

This prints an authorization URL. Open it in your browser, approve, then paste the `localhost:1455/auth/callback?...` URL back into the terminal.

### 5. Create config.toml

```bash
cat > /tmp/oab-config.toml <<'EOF'
[discord]
bot_token = "${DISCORD_BOT_TOKEN}"
allow_all_channels = true

[agent]
command = "/sandbox/openab-agent"
working_dir = "/sandbox"
env = { OPENAB_AGENT_OPENAI_MODEL = "gpt-5.4-mini" }

[pool]
max_sessions = 3
session_ttl_hours = 1

[reactions]
enabled = true
EOF
docker cp /tmp/oab-config.toml $CONTAINER:/sandbox/config.toml
```

> **Note:** The `${DISCORD_BOT_TOKEN}` env var expansion works only if the raw token is available in the sandbox environment. If using OpenShell providers (which inject reference tokens), hardcode the bot token directly in the config instead.

### 6. Set network policy

```bash
openshell policy update oab \
  --add-endpoint "discord.com:443:read-write:rest:enforce" \
  --add-endpoint "gateway.discord.gg:443:read-write:websocket:enforce" \
  --add-endpoint "cdn.discordapp.com:443:read-write:rest:enforce" \
  --add-endpoint "chatgpt.com:443:read-write:rest:enforce" \
  --add-endpoint "auth0.openai.com:443:read-write:rest:enforce"
```

### 7. Run OAB

```bash
# Inside sandbox (openshell sandbox connect oab)
sandbox$ cd /sandbox && ./openab run --config config.toml
```

Or from the host:

```bash
docker exec -d $CONTAINER bash -c "cd /sandbox && ./openab run --config config.toml"
```

## Credential Management

| Operation | Command |
|-----------|---------|
| List providers | `openshell provider list` |
| Delete a provider | `openshell provider delete discord` |
| Rotate a credential | Delete + recreate with new value |

Providers use `--type generic --credential KEY=VALUE` format. Credentials are injected as env vars at sandbox runtime.

## Network Policy

OpenShell sandboxes have **default-deny egress**. Use `openshell policy update` to allow specific endpoints:

```bash
# Add an endpoint
openshell policy update oab --add-endpoint "api.example.com:443:read-write:rest:enforce"

# View current policy
openshell policy get oab
```

Endpoint format: `host:port:access:protocol:mode`

### Required endpoints by agent backend

| Backend | Endpoints |
|---------|-----------|
| All | `discord.com:443`, `gateway.discord.gg:443`, `cdn.discordapp.com:443` |
| Native Agent (codex) | `chatgpt.com:443`, `auth0.openai.com:443` |
| Native Agent (anthropic) | `api.anthropic.com:443` |
| GitHub access | `api.github.com:443`, `github.com:443` |

## Port Forwarding

Add `--forward <port>` at sandbox creation:

```bash
openshell sandbox create --name oab \
  --provider discord \
  --forward 3000 \
  --forward 8080 \
  -- bash
```

Each forwarded port creates a tunnel: `localhost:<port>` on the host → `127.0.0.1:<port>` inside the sandbox.

## BYOC (Custom Image)

A pre-built Dockerfile is provided at [`openshell/Dockerfile`](../openshell/Dockerfile) with OAB and the native agent pre-installed:

```bash
openshell sandbox create --name oab \
  --from openshell/Dockerfile \
  --provider discord \
  -- bash
```

To customize the OAB version:

```bash
docker build --build-arg OAB_VERSION=openab-0.8.4-beta.10 -t oab-sandbox openshell/
openshell sandbox create --name oab \
  --from oab-sandbox \
  --provider discord \
  -- bash
```

## Cleanup

```bash
openshell sandbox delete oab
openshell provider delete discord
```

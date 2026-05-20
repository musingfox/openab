# openab-telegram

OpenAB + Telegram in a single pod — OAB agent, gateway, and Cloudflare Tunnel colocated.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│ Pod: openab-telegram                                        │
│                                                             │
│  ┌───────────┐    ws://localhost:8080/ws    ┌───────────┐   │
│  │  openab   │◄────────────────────────────►│  gateway  │   │
│  │  (agent)  │                              │  :8080    │   │
│  └─────┬─────┘                              └─────┬─────┘   │
│        │                                          │         │
│        │ /etc/openab/config.toml                  │         │
│        │ /home/agent (PVC)                        │         │
│        │                                          │         │
│  ┌─────┴──────────────────────────────────────────┴─────┐   │
│  │                    localhost                          │   │
│  └──────────────────────────┬───────────────────────────┘   │
│                             │                               │
│                    ┌────────┴────────┐                      │
│                    │   cloudflared   │                      │
│                    │   (tunnel)      │                      │
│                    └────────┬────────┘                      │
│                             │                               │
└─────────────────────────────┼───────────────────────────────┘
                              │ Cloudflare Tunnel
                              ▼
                 ┌────────────────────────┐
                 │  Cloudflare Edge       │
                 │  (bot.example.com)     │
                 └────────────┬───────────┘
                              │ HTTPS
                              ▼
                 ┌────────────────────────┐
                 │  Telegram API          │
                 │  (webhook delivery)    │
                 └────────────────────────┘
```

## Prerequisites

Run these on your **local machine** (or CI) — one-time setup, no browser required.

### 1. Create a Telegram bot

```bash
# Use the Telegram Bot API directly (no app needed):
curl "https://api.telegram.org/bot<YOUR_MAIN_BOT_TOKEN>/sendMessage" \
  -d "chat_id=@BotFather" -d "text=/newbot"

# Or message @BotFather in Telegram and save the token it returns.
# The token looks like: 123456789:ABCdefGHIjklMNOpqrsTUVwxyz
```

### 2. Create a Cloudflare Tunnel (fully headless)

```bash
# Install cloudflared
# macOS: brew install cloudflared
# Linux: curl -fsSL https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64 -o /usr/local/bin/cloudflared && chmod +x /usr/local/bin/cloudflared

# Authenticate with API token (no browser — create token at https://dash.cloudflare.com/profile/api-tokens or via Terraform)
# Required permissions: Account:Cloudflare Tunnel:Edit, Zone:DNS:Edit
export CLOUDFLARE_API_TOKEN="your-api-token"

# Or use service token auth:
cloudflared tunnel login  # only option if no API token; opens browser once

# Create the tunnel
cloudflared tunnel create my-telegram-bot

# Route DNS (creates CNAME: bot.example.com → <tunnel-id>.cfargotunnel.com)
cloudflared tunnel route dns my-telegram-bot bot.example.com

# Configure ingress (what the tunnel serves)
mkdir -p ~/.cloudflared
cat > ~/.cloudflared/config.yml <<EOF
tunnel: $(cloudflared tunnel info my-telegram-bot -o json | jq -r '.id')
ingress:
  - hostname: bot.example.com
    service: http://localhost:8080
  - service: http_status:404
EOF

# Get the tunnel token for helm (encapsulates credentials for remote mode)
cloudflared tunnel token my-telegram-bot
# → eyJ...  (pass this as cloudflareTunnelToken)
```

### 3. Set the Telegram webhook

```bash
export BOT_TOKEN="123456789:ABCdef..."
curl -s "https://api.telegram.org/bot${BOT_TOKEN}/setWebhook" \
  -d "url=https://bot.example.com/webhook/telegram"
```

## Quick Start

```bash
helm install my-bot ./charts/openab-telegram \
  --set telegramBotToken="<token-from-botfather>" \
  --set cloudflareTunnelToken="$(cloudflared tunnel token my-telegram-bot)" \
  --set webhookDomain=bot.example.com \
  --namespace openab --create-namespace
```

## Credential Management

Three options, from simplest to most secure:

### Option 1: `--set` (simple, least secure)

```bash
helm install my-bot ./charts/openab-telegram \
  --set telegramBotToken="123:ABC" \
  --set cloudflareTunnelToken="eyJ..." \
  --namespace openab --create-namespace
```

⚠️ Credentials are stored in Helm release metadata (a K8s Secret) and visible via `helm get values`. Suitable for dev/testing.

### Option 2: `--from-literal` (better)

Create the K8s Secret yourself, then reference it:

```bash
kubectl create secret generic my-bot-creds -n openab \
  --from-literal=telegram-bot-token="123:ABC" \
  --from-literal=cloudflare-tunnel-token="eyJ..."

helm install my-bot ./charts/openab-telegram \
  --set existingSecret=my-bot-creds \
  --namespace openab
```

Credentials don't appear in Helm values, but they briefly exist in shell history/process memory.

### Option 3: `--from-env-file` with process substitution (most secure)

Pull directly from an external secret manager (e.g., AWS Secrets Manager) without touching local disk:

```bash
kubectl create secret generic my-bot-creds -n openab \
  --from-env-file=<(aws secretsmanager get-secret-value \
    --secret-id oab --query SecretString --output text | \
    jq -r '{"telegram-bot-token": .telegramBotToken, "cloudflare-tunnel-token": .cloudflareTunnelToken} | to_entries[] | "\(.key)=\(.value)"')

helm install my-bot ./charts/openab-telegram \
  --set existingSecret=my-bot-creds \
  --namespace openab
```

Credentials flow from AWS → K8s Secret without touching local disk or shell variables. The process substitution (`<(...)`) is ephemeral.

> **Expected Secret keys:** `telegram-bot-token`, `cloudflare-tunnel-token`

## Post-Install

Authenticate the agent (device flow — outputs a URL and code to paste, no browser on the server needed):

```bash
kubectl exec -it deployment/my-bot -n openab -c openab -- openab login --use-device-flow
kubectl rollout restart deployment/my-bot -n openab
```

## Values

| Key | Required | Default | Description |
|-----|----------|---------|-------------|
| `telegramBotToken` | Yes* | `""` | Telegram bot token |
| `cloudflareTunnelToken` | Yes* | `""` | Cloudflare Tunnel token |
| `existingSecret` | No | `""` | Pre-existing Secret name (skips token fields) |
| `webhookDomain` | No | `""` | Shown in post-install notes |
| `image.repository` | No | `ghcr.io/openabdev/openab` | Agent image |
| `image.tag` | No | `appVersion` | Agent image tag |
| `gateway.tag` | No | `v0.5.0` | Gateway image tag |
| `agent.command` | No | `kiro-cli` | Agent command |
| `platform.allowAllUsers` | No | `true` | Allow any Telegram user |
| `platform.allowedUsers` | No | `[]` | Allowed Telegram user IDs |
| `persistence.enabled` | No | `true` | Enable PVC for agent state |
| `persistence.size` | No | `1Gi` | PVC size |

*Required unless `existingSecret` is set.

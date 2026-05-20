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

## Quick Start

```bash
helm install my-bot ./charts/openab-telegram \
  --set telegramBotToken="123:ABC" \
  --set cloudflareTunnelToken="eyJ..." \
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

1. Set the Telegram webhook:
   ```bash
   curl "https://api.telegram.org/bot<TOKEN>/setWebhook" \
     -d "url=https://YOUR_TUNNEL_DOMAIN/webhook/telegram"
   ```

2. Authenticate the agent:
   ```bash
   kubectl exec -it deployment/my-bot -n openab -c openab -- kiro-cli login --use-device-flow
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

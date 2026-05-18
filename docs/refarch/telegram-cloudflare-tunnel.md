# Reference Architecture: Telegram via Cloudflare Tunnel

Deploy OpenAB on K3s with the Custom Gateway receiving Telegram webhooks through a Cloudflare Tunnel — no public IP, no ingress controller, no TLS certificates required.

## Architecture

```
Telegram Cloud
    │ HTTPS POST
    ▼
Cloudflare Edge (bot.example.com)
    │ Tunnel
    ▼
┌─────────────────────────────────────────┐
│  Gateway Pod                            │
│  ┌───────────────┐  ┌────────────────┐  │
│  │ cloudflared   │──│ gateway :8080  │  │
│  │ (sidecar)     │  │                │  │
│  └───────────────┘  └───────▲────────┘  │
└─────────────────────────────│───────────┘
                              │ WebSocket (cluster-internal)
                    ┌─────────┴─────────┐
                    │     OAB Pod       │
                    └───────────────────┘
```

- **Telegram** sends webhook POSTs to the Cloudflare edge hostname.
- **Cloudflare Tunnel** routes traffic to the `cloudflared` sidecar inside the cluster.
- **Custom Gateway** receives the POST, normalizes it, and forwards to OAB over WebSocket.
- **OAB** connects outbound to the gateway — no inbound ports needed.

## Prerequisites

| Requirement | Notes |
|-------------|-------|
| K3s cluster | Any single-node or multi-node K3s setup |
| Helm 3 | Installed on the node or a workstation with kubeconfig access |
| Cloudflare account | Free plan is sufficient |
| Telegram Bot Token | Create via [@BotFather](https://t.me/BotFather) |
| Domain on Cloudflare | DNS managed by Cloudflare |

## Step 1: Create a Cloudflare Tunnel

1. Go to **Zero Trust → Networks → Tunnels → Create a tunnel**
2. Name it (e.g. `openab-telegram`)
3. Copy the **tunnel token**
4. Add a **public hostname**:
   - Subdomain: your choice (e.g. `bot`)
   - Domain: your Cloudflare-managed domain
   - Service: `http://localhost:8080`

## Step 2: Deploy with Helm

```bash
cd openab

RELEASE_NAME="my-openab"

helm upgrade --install "$RELEASE_NAME" ./charts/openab \
  --set agents.kiro.discord.enabled=false \
  --set agents.kiro.gateway.enabled=true \
  --set agents.kiro.gateway.deploy=true \
  --set agents.kiro.gateway.url="ws://${RELEASE_NAME}-kiro-gateway:8080/ws" \
  --set agents.kiro.gateway.platform=telegram \
  --set agents.kiro.gateway.image="ghcr.io/openabdev/openab-gateway" \
  --set agents.kiro.gateway.tag="0.4.0" \
  --set-literal agents.kiro.gateway.telegram.botToken="<TELEGRAM_BOT_TOKEN>" \
  --set agents.kiro.gateway.extraContainers[0].name=cloudflared \
  --set agents.kiro.gateway.extraContainers[0].image="cloudflare/cloudflared:2024.12.2" \
  --set agents.kiro.gateway.extraContainers[0].args[0]="tunnel" \
  --set agents.kiro.gateway.extraContainers[0].args[1]="--no-autoupdate" \
  --set agents.kiro.gateway.extraContainers[0].args[2]="run" \
  --set-literal agents.kiro.gateway.extraContainers[0].env[0].name=TUNNEL_TOKEN \
  --set-literal agents.kiro.gateway.extraContainers[0].env[0].value="<CLOUDFLARE_TUNNEL_TOKEN>" \
  --namespace openab --create-namespace
```

## Step 3: Authenticate the Agent

```bash
kubectl exec -it deployment/${RELEASE_NAME}-kiro -n openab -- kiro-cli login --use-device-flow
```

After login, restart the pod to pick up credentials:

```bash
kubectl rollout restart deployment/${RELEASE_NAME}-kiro -n openab
```

## Step 4: Set the Telegram Webhook

```bash
curl "https://api.telegram.org/bot<TELEGRAM_BOT_TOKEN>/setWebhook" \
  -d "url=https://bot.example.com/webhook/telegram"
```

Verify:

```bash
curl "https://api.telegram.org/bot<TELEGRAM_BOT_TOKEN>/getWebhookInfo"
```

## Resulting Resources

```
$ kubectl get pods -n openab
NAME                                    READY   STATUS    AGE
my-openab-kiro-xxxxx-yyyyy              1/1     Running   ...
my-openab-kiro-gateway-xxxxx-yyyyy      2/2     Running   ...
```

The gateway pod runs 2 containers: `gateway` and `cloudflared`.

## Configuration

The rendered `config.toml` for the OAB agent:

```toml
[agent]
command = "kiro-cli"
args = ["acp", "--trust-all-tools"]
working_dir = "/home/agent"

[pool]
max_sessions = 10
session_ttl_hours = 24

[reactions]
enabled = true
remove_after_reply = false

[gateway]
url = "ws://my-openab-kiro-gateway:8080/ws"
platform = "telegram"
allow_all_channels = true
allowed_channels = []
# ⚠️ Recommended: restrict to specific Telegram user IDs
allow_all_users = false
allowed_users = ["<YOUR_TELEGRAM_USER_ID>"]
```

## Restricting Access

To limit which Telegram users can interact with the bot:

```bash
helm upgrade $RELEASE_NAME ./charts/openab \
  ... \
  --set agents.kiro.gateway.allowAllUsers=false \
  --set-string agents.kiro.gateway.allowedUsers[0]="<TELEGRAM_USER_ID>"
```

## Why Cloudflare Tunnel?

- **No public IP required** — the K3s node can be behind NAT or a firewall.
- **No TLS management** — Cloudflare terminates TLS at the edge.
- **No ingress controller config** — bypasses Traefik/nginx entirely.
- **Sidecar pattern** — `cloudflared` runs alongside the gateway in the same pod, routing to `localhost:8080`.

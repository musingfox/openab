# Reference Architecture: Telegram via Cloudflare Tunnel

Deploy OpenAB with the Custom Gateway on a K3s cluster, exposing the Telegram webhook through a Cloudflare Tunnel sidecar — no ingress controller or public ports required.

## Architecture

```
Telegram  ──POST──▶  Cloudflare Edge (your_custom.domain.com)
                          │
                     Tunnel (QUIC)
                          │
                          ▼
              ┌───────────────────────┐
              │  Gateway Pod (K3s)    │
              │                       │
              │  ┌─────────────────┐  │
              │  │ cloudflared     │  │  ← sidecar container
              │  │ (tunnel client) │  │
              │  └────────┬────────┘  │
              │           │ localhost  │
              │  ┌────────▼────────┐  │
              │  │ openab-gateway  │  │  ← :8080, telegram adapter
              │  │                 │  │
              │  └────────┬────────┘  │
              └───────────│───────────┘
                          │ WebSocket (cluster-internal)
              ┌───────────▼───────────┐
              │  OAB Pod              │
              │  (kiro-cli / agent)   │
              └───────────────────────┘
```

## Prerequisites

- K3s cluster (no ingress controller needed)
- Cloudflare account with a domain (DNS managed by Cloudflare)
- Telegram bot token from [@BotFather](https://t.me/BotFather)
- Helm 3
- `openab` Helm repo added:
  ```bash
  helm repo add openab https://openabdev.github.io/openab
  helm repo update
  ```

## Step 1: Create a Cloudflare Tunnel

1. Go to [Cloudflare Zero Trust](https://one.dash.cloudflare.com/) → Networks → Tunnels → **Create a tunnel**
2. Choose **Cloudflared** as the connector
3. Name the tunnel (e.g. `openab-telegram`)
4. Copy the tunnel token (starts with `eyJ...`)
5. Add a **Public Hostname**:
   - Subdomain + Domain: e.g. `your_custom.domain.com`
   - Service: `http://localhost:8080`

## Step 2: Create the Telegram Bot Token Secret

```bash
kubectl create namespace openab

kubectl create secret generic openab-kiro-gateway \
  --namespace openab \
  --from-literal=telegram-bot-token="YOUR_TELEGRAM_BOT_TOKEN"
```

## Step 3: Helm Install

```bash
helm install openab openab/openab \
  --namespace openab \
  --values values.yaml
```

**values.yaml:**

```yaml
agents:
  kiro:
    discord:
      enabled: false
    gateway:
      enabled: true
      deploy: true
      platform: telegram
      url: "ws://openab-kiro-gateway:8080/ws"
      tag: "0.4.0"  # check ghcr.io/openabdev/openab-gateway for latest
      telegram:
        botToken: "placeholder"  # triggers template rendering; actual token from Secret
```

> **Note:** As of chart version 0.8.2, the gateway template does not natively inject `TELEGRAM_BOT_TOKEN` from the Secret or support `extraContainers`. A manual patch is required (Step 4). Future chart versions will handle this natively.

## Step 4: Patch the Gateway Deployment

Inject the Telegram token from the Secret and add the cloudflared sidecar:

```bash
kubectl patch deployment openab-kiro-gateway -n openab --type=json -p '[
  {
    "op": "add",
    "path": "/spec/template/spec/containers/0/env/-",
    "value": {
      "name": "TELEGRAM_BOT_TOKEN",
      "valueFrom": {
        "secretKeyRef": {
          "name": "openab-kiro-gateway",
          "key": "telegram-bot-token"
        }
      }
    }
  },
  {
    "op": "add",
    "path": "/spec/template/spec/containers/-",
    "value": {
      "name": "cloudflared",
      "image": "cloudflare/cloudflared:latest",
      "args": [
        "tunnel",
        "--no-autoupdate",
        "run",
        "--token",
        "YOUR_TUNNEL_TOKEN"
      ]
    }
  }
]'
```

## Step 5: Set the Telegram Webhook

```bash
curl "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/setWebhook?url=https://your_custom.domain.com/webhook/telegram"
```

Verify:

```bash
curl "https://api.telegram.org/bot${TELEGRAM_BOT_TOKEN}/getWebhookInfo"
```

## Step 6: Authenticate the Agent

```bash
kubectl exec -it deployment/openab-kiro -n openab -- kiro-cli login --use-device-flow
```

Follow the device-flow URL to complete authentication, then restart:

```bash
kubectl rollout restart deployment/openab-kiro -n openab
```

## Verification

Check that all components are healthy:

```bash
# Both containers running (gateway + cloudflared)
kubectl get pods -n openab

# Gateway logs — should show "telegram adapter enabled" and "OAB client connected"
kubectl logs deployment/openab-kiro-gateway -n openab -c gateway

# Cloudflared logs — should show "Registered tunnel connection"
kubectl logs deployment/openab-kiro-gateway -n openab -c cloudflared
```

Send a message to your Telegram bot — you should see the gateway log the incoming webhook and forward it to OAB.

## Security Considerations

- **Webhook validation:** Set `TELEGRAM_SECRET_TOKEN` on the gateway and configure it in the Telegram webhook to validate inbound requests.
- **User restriction (recommended):** By default the chart sets `allow_all_users = true`. For production, restrict access in your Helm values:
  ```yaml
  agents:
    kiro:
      gateway:
        allowAllUsers: false
        allowedUsers:
          - "YOUR_TELEGRAM_USER_ID"
  ```
- **WebSocket auth:** Set `GATEWAY_WS_TOKEN` to authenticate the OAB → Gateway WebSocket connection.
- **Tunnel token:** Store the Cloudflare tunnel token in a Kubernetes Secret rather than inline in the deployment spec for production use.

## Upgrade Notes

The manual `kubectl patch` in Step 4 will be overwritten by `helm upgrade`. After upgrading the chart, re-apply the patch. Once the chart natively supports `gateway.telegram.botToken` Secret injection and `gateway.extraContainers`, the patch will no longer be needed.

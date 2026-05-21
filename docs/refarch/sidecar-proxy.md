# Reference Architecture: OAuth Sidecar Proxy

> **Note:** For xAI/Grok models, OpenCode ≥1.15.0 supports native xAI OAuth
> (browser + device-code). The sidecar proxy is no longer required for OpenCode
> deployments. See [docs/xai-proxy.md](../xai-proxy.md) for the recommended
> approach.

This document describes the **sidecar proxy pattern** used by `xai-proxy` to
provide OAuth-authenticated access to xAI's SuperGrok API for any
OpenAI-compatible agent.

## When to use this pattern

- Running agents that **don't** have built-in xAI OAuth (e.g. Hermes, custom agents)
- Centralizing token management across multiple containers in a pod
- Needing a single OAuth session shared by several processes

## Architecture

```
┌─ Kubernetes Pod ──────────────────────────────────────────────┐
│                                                               │
│  agent container (any OpenAI-compatible client)               │
│               │  POST /v1/chat/completions                    │
│               │  (no auth header needed)                      │
│               ▼                                               │
│         xai-proxy :9090                                       │
│           • Reads OAuth token from PVC                        │
│           • Injects Authorization: Bearer header              │
│           • Auto-refreshes 120s before expiry                 │
│               │                                               │
│  PVC: /home/agent/.openab/xai-proxy/tokens.json               │
└───────────────┼───────────────────────────────────────────────┘
                ▼
        https://api.x.ai/v1  (SuperGrok)
```

## How it works

1. **One-time login** — Run `xai-proxy login-device` on any machine with a
   browser. This performs OAuth PKCE device-code flow against xAI and writes
   tokens to `~/.xai-proxy/tokens.json`.

2. **Token seeding** — An init container copies the token from a K8s Secret
   into the PVC on first boot.

3. **Proxy** — `xai-proxy serve` listens on `:9090`, reads the token file,
   injects the Bearer header into every upstream request to `api.x.ai/v1`.

4. **Auto-refresh** — The proxy refreshes the OAuth token 120s before expiry
   and writes it back to the PVC (survives pod restarts).

## Helm deployment

```bash
# 1. Login locally
xai-proxy login-device

# 2. Create K8s secret
kubectl create secret generic xai-proxy-tokens \
  --from-file=tokens.json=$HOME/.xai-proxy/tokens.json

# 3. Deploy with sidecar
helm install openab openab/openab \
  --set agents.mybot.command=opencode \
  --set-json 'agents.mybot.args=["acp"]' \
  --set agents.mybot.image=ghcr.io/openabdev/openab-opencode \
  --set-json 'agents.mybot.extraContainers=[{"name":"xai-proxy","image":"ghcr.io/openabdev/xai-proxy:latest","args":["serve","--bind","0.0.0.0"],"env":[{"name":"XAI_PROXY_TOKEN_PATH","value":"/home/agent/.openab/xai-proxy/tokens.json"}],"ports":[{"containerPort":9090}],"volumeMounts":[{"name":"data","mountPath":"/home/agent"}]}]' \
  --set-json 'agents.mybot.extraInitContainers=[{"name":"copy-tokens","image":"busybox","command":["sh","-c","mkdir -p /dest/.openab/xai-proxy && cp /src/tokens.json /dest/.openab/xai-proxy/tokens.json"],"volumeMounts":[{"name":"xai-tokens-src","mountPath":"/src","readOnly":true},{"name":"data","mountPath":"/dest"}]}]' \
  --set-json 'agents.mybot.extraVolumes=[{"name":"xai-tokens-src","secret":{"secretName":"xai-proxy-tokens"}}]'
```

The agent's `opencode.json` points to the local proxy:

```json
{
  "provider": {
    "xai": {
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "http://localhost:9090/v1", "apiKey": "dummy" },
      "models": { "grok-4.3": { "name": "Grok 4.3" } }
    }
  }
}
```

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `XAI_PROXY_TOKEN_PATH` | `~/.xai-proxy/tokens.json` | Token file location |
| `RUST_LOG` | `xai_proxy=info` | Log verbosity |

## Limitations

- Only useful for agents without native xAI OAuth support
- Browser OAuth (`xai-proxy login`) may be blocked by Cloudflare — prefer `login-device`
- `codex-acp` and `claude-agent-acp` use proprietary auth and cannot use this proxy

## See also

- [xai-proxy source](../../xai-proxy/) — Rust implementation
- [docs/xai-proxy.md](../xai-proxy.md) — Quick-start guide (now recommends native OAuth)

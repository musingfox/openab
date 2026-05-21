# xAI / SuperGrok Integration

## Recommended: Native xAI OAuth (OpenCode ≥1.15.0)

OpenCode now has **built-in xAI OAuth support** — no sidecar proxy needed.

### Setup

1. Use `Dockerfile.opencode` with OpenCode ≥1.15.0 (which includes native xAI OAuth).
2. Run `/connect` inside OpenCode and select **xAI Grok OAuth (Headless / Remote / VPS)**.
3. Approve the device-code on any browser.
4. Select your model with `/models` (e.g. `grok-4.3`).

OpenCode handles token storage and auto-refresh internally.

### Helm deployment (native OAuth)

```bash
# 1. Run opencode interactively once to complete device-code login,
#    then copy the auth file:
kubectl cp <pod>:/home/node/.local/share/opencode/auth.json ./auth.json

# 2. Create secret from the auth file
kubectl create secret generic opencode-xai-auth \
  --from-file=auth.json=./auth.json

# 3. Deploy — no sidecar needed
helm install openab openab/openab \
  --set agents.mybot.command=opencode \
  --set-json 'agents.mybot.args=["acp"]' \
  --set agents.mybot.image=ghcr.io/openabdev/openab-opencode \
  --set-json 'agents.mybot.extraVolumes=[{"name":"xai-auth","secret":{"secretName":"opencode-xai-auth"}}]' \
  --set-json 'agents.mybot.extraVolumeMounts=[{"name":"xai-auth","mountPath":"/home/node/.local/share/opencode/auth.json","subPath":"auth.json"}]'
```

### opencode.json

```json
{
  "$schema": "https://opencode.ai/config.json",
  "model": "xai/grok-4.3"
}
```

No custom provider block needed — OpenCode discovers xAI natively when auth is present.

---

## Alternative: xai-proxy sidecar (legacy)

For agents **without** native xAI OAuth (Hermes, custom agents), or for
multi-container pods sharing a single OAuth session, the `xai-proxy` sidecar
is still available.

See [docs/refarch/sidecar-proxy.md](refarch/sidecar-proxy.md) for the full
architecture and deployment guide.

### Quick start

```bash
xai-proxy login-device          # one-time device-code login
xai-proxy serve --port 9090     # start proxy

# Point any OpenAI-compatible client at the proxy
export OPENAI_BASE_URL=http://127.0.0.1:9090/v1
export OPENAI_API_KEY=dummy
```

## Comparison

| | Native OAuth | xai-proxy sidecar |
|---|---|---|
| **Requires** | OpenCode ≥1.15.0 | Any OpenAI-compatible agent |
| **Extra container** | No | Yes |
| **Token management** | Built into OpenCode | Proxy handles refresh |
| **Multi-agent sharing** | Each agent needs own auth | Single proxy serves all |
| **Complexity** | Low | Medium |

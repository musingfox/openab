# Hermes Agent

[Hermes Agent](https://github.com/NousResearch/hermes-agent) by Nous Research supports ACP natively via the `hermes acp` subcommand (or the `hermes-acp` binary).

Hermes acts as a multi-provider inference gateway — it handles OAuth token lifecycle, credential storage, and provider routing so OAB agents don't need to manage auth directly.

## Docker Image

```bash
docker build -f Dockerfile.hermes -t openab-hermes:latest .
```

The image installs Hermes Agent via the official install script.

## Helm Install

```bash
helm install openab openab/openab \
  --set agents.kiro.enabled=false \
  --set agents.hermes.discord.enabled=true \
  --set agents.hermes.discord.botToken="$DISCORD_BOT_TOKEN" \
  --set-string 'agents.hermes.discord.allowedChannels[0]=YOUR_CHANNEL_ID' \
  --set agents.hermes.image=ghcr.io/openabdev/openab-hermes:latest \
  --set agents.hermes.command=hermes-acp \
  --set agents.hermes.workingDir=/home/agent
```

> Set `agents.kiro.enabled=false` to disable the default Kiro agent.

## Manual config.toml

```toml
[agent]
command = "hermes-acp"
working_dir = "/home/agent"
```

## Authentication

Hermes supports 30+ providers. Authenticate inside the pod:

```bash
kubectl exec -it <pod> -- hermes auth add xai-oauth    # xAI Grok (SuperGrok $30/mo)
kubectl exec -it <pod> -- hermes auth add nous         # Nous Portal
kubectl exec -it <pod> -- hermes model                 # Interactive provider picker
```

### xAI Grok OAuth (Recommended)

> ⚠️ **Requires an active [SuperGrok paid subscription](https://x.ai/grok) ($30/mo).** Auth will succeed without one, but the API silently returns empty responses — the bot appears to work but never replies.

xAI Grok OAuth uses a loopback redirect flow — the callback listener binds `127.0.0.1:56121` inside the pod. You need a port-forward so your browser's redirect reaches the pod:

```bash
# Terminal 1: port-forward
kubectl port-forward deployment/<your-deployment> 56121:56121

# Terminal 2: run auth
kubectl exec -it deployment/<your-deployment> -- hermes auth add xai-oauth --no-browser
```

1. Copy the printed authorize URL → open in your local browser
2. Approve access on accounts.x.ai
3. Browser redirects to `127.0.0.1:56121/callback` → port-forward delivers it to the pod
4. Terminal shows "Login successful!"

After auth, set the default model:

```bash
kubectl exec <pod> -- hermes config set model.provider xai-oauth
kubectl exec <pod> -- hermes config set model.default grok-4.3
```

> **Note:** Tokens are stored in `~/.hermes/auth.json` and auto-refresh in the background.

### Providers That Don't Need Port-Forward

| Provider | Auth Method |
|----------|-------------|
| Anthropic (Claude Pro/Max) | Paste-the-code flow |
| OpenAI Codex (ChatGPT Plus/Pro) | Device code flow |
| MiniMax, Nous Portal | Device code flow |
| xAI Grok, Spotify | Loopback OAuth (port-forward required) |

### Supported Providers (via OAuth)

| Provider | Auth Command | Cost Model |
|----------|-------------|------------|
| xAI Grok | `hermes auth add xai-oauth` | SuperGrok subscription ($30/mo) |
| OpenAI Codex | `hermes model` → OpenAI Codex | ChatGPT subscription |
| GitHub Copilot | `hermes model` → GitHub Copilot | Copilot subscription |
| Google Gemini | `hermes model` → Google Gemini (OAuth) | Free tier available |
| Anthropic | `hermes model` → Anthropic | Claude Max + extra credits |
| Nous Portal | `hermes auth add nous` | Nous subscription |

### Supported Providers (via API Key)

Any provider can also be configured with an API key via environment variables:

```toml
[agent]
command = "hermes-acp"
working_dir = "/home/agent"
env = { XAI_API_KEY = "${XAI_API_KEY}" }
```

## Provider Switching

Switch providers without restarting the pod:

```bash
kubectl exec -it <pod> -- hermes model
```

## Credential Persistence

Hermes stores OAuth tokens in `~/.hermes/`. The OpenAB Helm chart's default persistence covers this automatically (PVC mounted at `workingDir`).

If deploying manually (without the Helm chart), mount persistent storage at `/home/agent` or `/home/agent/.hermes`:

```yaml
volumes:
  - name: hermes-credentials
    persistentVolumeClaim:
      claimName: hermes-credentials-pvc
volumeMounts:
  - name: hermes-credentials
    mountPath: /home/agent/.hermes
```

## Advantages

- **Cost**: SuperGrok $30/mo flat rate vs pay-per-token API pricing
- **Multi-provider**: 30+ providers accessible through one agent
- **Zero auth complexity**: Hermes handles OAuth + token refresh
- **Multi-modal**: TTS, image gen, video gen via the same OAuth token
- **Fallback chains**: Auto-switch providers on failure

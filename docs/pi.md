# Pi Coding Agent

Pi is a minimalist, terminal-based AI coding agent designed for extensibility and developer control. It supports various modes and interfaces, including running inside the Zed editor or other Agent Client Protocol (ACP) clients.

In OpenAB, Pi runs via the `pi-acp` adapter, which translates ACP stdio JSON-RPC messages into Pi RPC commands.

## Use Cases

1. **High Context Utilization Tasks**: Because Pi's system prompt is extremely small and it only exposes 4 core tools (`read`, `write`, `edit`, `bash`), it leaves maximum context window space for code and project files. It is perfect for large context tasks where other bloated agents run out of token space or become slow.
2. **Flexible Multi-Model Switching**: Pi supports switching between 15+ LLM providers mid-session. It is ideal for workflows where you want to use different models for different steps (e.g., Claude 3.5 Sonnet for logic reasoning, DeepSeek-Coder for quick edits, and Gemini for massive context ingestion).
3. **Exploratory / Branching Workflows**: Pi saves sessions as tree structures. If you want the agent to explore different implementation paths without losing previous state, Pi allows you to easily branch sessions.
4. **Secure Team Environments**: Combined with `openab-auth-proxy`, you can configure OpenAB to run Pi with centralized OAuth tokens (like xAI OAuth) rather than storing developer API keys in plain text on local machines.

## Advantages

- **Minimalism & Speed**: Very low system prompt overhead, saving up to 80% of standard system prompt tokens compared to other feature-rich agents.
- **Client-Side & Open-Source**: Pi runs completely locally/locally-hosted. It is MIT-licensed, giving you total data sovereignty.
- **Extensible via skills**: Developers can write skills in TypeScript and ask Pi to install or even generate them dynamically.

## Docker Image

```bash
docker build -f Dockerfile.pi -t openab-pi:latest .
```

The image installs `pi-acp` and `@earendil-works/pi-coding-agent` globally via npm.

## Helm Install

```bash
helm install openab openab/openab \
  --set agents.kiro.enabled=false \
  --set agents.pi.discord.enabled=true \
  --set agents.pi.discord.botToken="$DISCORD_BOT_TOKEN" \
  --set-string 'agents.pi.discord.allowedChannels[0]=YOUR_CHANNEL_ID' \
  --set agents.pi.image=ghcr.io/openabdev/openab-pi:latest \
  --set agents.pi.command=pi-acp \
  --set agents.pi.workingDir=/home/node
```

## Manual config.toml

```toml
[agent]
command = "pi-acp"
working_dir = "/home/node"
env = { ANTHROPIC_API_KEY = "${ANTHROPIC_API_KEY}" }
```

## Authentication

To authenticate with providers:

- **Environment variables**: Pass `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc., via the agent's `env` configuration or `secretEnv`.
- **Interactive login**: Run `/login` inside the Pi agent:
  ```bash
  kubectl exec -it deployment/openab-pi -- pi /login
  ```

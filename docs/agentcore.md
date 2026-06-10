# AgentCore Runtime Backend

Run your coding agent (Kiro, Claude Code, Codex, etc.) remotely on [Amazon Bedrock AgentCore Runtime](https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/runtime.html) instead of bundling it inside the OAB container.

## Why

- **No coding CLI in your OAB image** — smaller, faster pulls, simpler upgrades
- **True isolation** — each agent session runs in its own Firecracker microVM
- **Persistent workspace** — `/mnt/workspace` survives across turns (14-day retention)
- **Background execution** — agents survive pod restarts
- **Multi-agent routing** — one OAB routes to N runtimes by config

## Quick Start

```toml
# config.toml
[discord]
bot_token = "${DISCORD_BOT_TOKEN}"

[agentcore]
runtime_arn = "arn:aws:bedrock-agentcore:us-east-1:123456789012:runtime/my-kiro-agent"
region = "us-east-1"
```

That's it. OAB auto-spawns the bundled `agentcore-acp` adapter.

## Prerequisites

1. **An AgentCore Runtime** with your coding agent deployed (see [Deploying a Kiro Runtime](#deploying-a-kiro-runtime) below)
2. **AWS credentials** on the OAB pod with `bedrock-agentcore:InvokeAgentRuntime` permission
3. **`uv`** installed (for running the adapter script)

## Config Reference

```toml
[agentcore]
runtime_arn = "arn:aws:bedrock-agentcore:us-east-1:123456789012:runtime/my-agent"  # required
region = "us-east-1"           # default: us-east-1
cancel_strategy = "stop"       # "stop" (default) or "noop"
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `runtime_arn` | yes | — | AgentCore Runtime ARN |
| `region` | no | `us-east-1` | AWS region |
| `cancel_strategy` | no | `stop` | What to do on cancel: `stop` terminates the session, `noop` ignores |

If you need full control, use `[agent]` directly:

```toml
[agent]
command = "uv"
args = ["run", "--script", "agentcore-acp/agentcore_acp.py", "--runtime-arn", "arn:aws:...", "--region", "us-east-1"]
```

## Docker Image

Use `ghcr.io/openabdev/openab-agentcore` — a minimal image (~50MB) with only OAB + the adapter. No coding CLI bundled.

```bash
docker pull ghcr.io/openabdev/openab-agentcore:latest
```

## Deploying a Kiro Runtime

> **Note:** AWS does not currently offer a pre-built managed Kiro runtime. You build and deploy the container yourself. This applies to all coding agents (Claude Code, Codex, Cursor, etc.) — AgentCore hosts your container, it doesn't provide one. This may change as AgentCore evolves.

### 1. Build the container (arm64 required)

```dockerfile
FROM public.ecr.aws/amazonlinux/amazonlinux:2023
RUN dnf install -y git curl python3 python3-pip unzip && dnf clean all
RUN useradd -m -d /home/agent -u 1000 agent

# Install kiro-cli
USER agent
RUN curl -fsSL https://cli.kiro.dev/install | bash
USER root

RUN pip3 install boto3
COPY healthcheck.py /app/healthcheck.py
COPY run.sh /app/run.sh
RUN chmod +x /app/run.sh

ENV PATH="/home/agent/.local/bin:${PATH}"
WORKDIR /app
EXPOSE 8080
USER agent
CMD ["python3", "/app/healthcheck.py"]
```

### 2. Push to ECR and create the runtime

```bash
# Push image
aws ecr create-repository --repository-name agentcore-kiro --region us-east-1
docker buildx build --platform linux/arm64 -t <ACCOUNT>.dkr.ecr.us-east-1.amazonaws.com/agentcore-kiro:latest . --push

# Create runtime
aws bedrock-agentcore-control create-agent-runtime \
  --agent-runtime-name kiro_agent \
  --agent-runtime-artifact '{"containerConfiguration":{"containerUri":"<ACCOUNT>.dkr.ecr.us-east-1.amazonaws.com/agentcore-kiro:latest"}}' \
  --role-arn "arn:aws:iam::<ACCOUNT>:role/agentcore-execution-role" \
  --network-configuration '{"networkMode":"PUBLIC"}' \
  --protocol-configuration '{"serverProtocol":"HTTP"}' \
  --region us-east-1
```

### 3. Store API key in Token Vault

```bash
aws bedrock-agentcore-control create-workload-identity --name kiro-coding-agent --region us-east-1
aws bedrock-agentcore-control create-api-key-credential-provider \
  --name kiro-api-key --api-key "$KIRO_API_KEY" --region us-east-1
```

The runtime fetches the key at boot — no plaintext secrets in env vars or config.

## How It Works

```
┌─────────┐       ┌─────────┐  ACP   ┌───────────────┐  SDK   ┌─────────────────────┐
│ Discord │──────▶│   OAB   │───────▶│ agentcore-acp │──────▶│  AgentCore Runtime   │
│  Slack  │       │         │ stdio  │  (Python)     │       │  (Firecracker μVM)   │
└─────────┘       └─────────┘        └───────────────┘       │  ┌───────────────┐   │
                                                               │  │ Kiro/Claude/… │   │
                                                               │  └───────────────┘   │
                                                               └─────────────────────┘
```

1. OAB spawns `agentcore-acp` as a subprocess (same as kiro-cli or claude-agent-acp)
2. On each message, adapter calls `invoke_agent_runtime` with the prompt
3. AgentCore routes to the microVM, runs the coding agent, streams response
4. Adapter translates the response back to ACP notifications on stdout
5. OAB renders in Discord/Slack/Telegram as usual

## Session Memory

Each Discord/Slack thread maps to a deterministic `runtimeSessionId`. AgentCore keeps the same microVM alive for 15 minutes (configurable up to 8 hours). The persistent filesystem means:

- Kiro's conversation history survives across turns (via `--resume`)
- Git repos, node_modules, build caches all persist
- No re-clone on every message

## IAM Policy

Minimum permissions for the OAB pod:

```json
{
  "Effect": "Allow",
  "Action": ["bedrock-agentcore:InvokeAgentRuntime"],
  "Resource": ["arn:aws:bedrock-agentcore:us-east-1:<ACCOUNT>:runtime/*"]
}
```

## Comparison

| | Local ACP (default) | AgentCore |
|---|---|---|
| Agent location | Same container | Remote microVM |
| Image size | ~500MB+ | ~50MB (agentcore variant) |
| Session state | In-memory (lost on restart) | Persistent filesystem (14 days) |
| Parallelism | Shared CPU | Independent microVM per session |
| Cold start | None | ~5-15s first invoke |
| Cost | Always-on pod | Pay per CPU-second |

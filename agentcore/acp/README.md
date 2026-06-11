# agentcore-acp

ACP stdio adapter for [Amazon Bedrock AgentCore Runtime](https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/runtime.html).

Bridges OAB's ACP JSON-RPC protocol to AgentCore's `InvokeAgentRuntime` streaming API. **Zero OAB code changes required.**

## Architecture

```
OAB ──ACP stdio──► agentcore-acp ──AWS SDK──► AgentCore Runtime (microVM)
```

## Usage

### With OpenAB (config.toml)

```toml
[agent]
command = "agentcore-acp"
args = ["--runtime-arn", "arn:aws:bedrock-agentcore:us-west-2:123456789012:runtime/my-agent", "--region", "us-west-2"]
working_dir = "/home/agent"
```

### Standalone (for testing)

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/"}}' | \
  agentcore-acp --runtime-arn arn:aws:bedrock-agentcore:us-west-2:123456789012:runtime/my-agent
```

## Installation

```bash
pip install -e .
```

Or with Docker:

```bash
docker build -t agentcore-acp .
```

## CLI Options

| Flag | Default | Description |
|------|---------|-------------|
| `--runtime-arn` | (required) | AgentCore Runtime ARN |
| `--region` | `us-west-2` | AWS region |
| `--cancel-strategy` | `stop` | `noop` (ignore cancel) or `stop` (terminate session) |

## How It Works

1. OAB spawns `agentcore-acp` as a subprocess
2. On `session/prompt`, the adapter parses `<sender_context>` from the prompt to extract `thread_id`
3. Builds a deterministic `runtimeSessionId` (`oab-{platform}-thread-{thread_id}`, ≥33 chars)
4. Calls `InvokeAgentRuntime` with streaming response
5. Translates SSE `data:` lines → ACP `notifications/content` JSON-RPC notifications
6. OAB displays the response in Discord/Slack as usual

## Requirements

- Python ≥ 3.11
- AWS credentials (IRSA, instance profile, or env vars)
- `bedrock-agentcore:InvokeAgentRuntime` IAM permission

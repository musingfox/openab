# ADR: AgentCore Runtime Backend

- **Status:** Proposed
- **Date:** 2026-06-10
- **Author:** @chaodu-agent

---

## 1. Context & Motivation

Today, OpenAB dispatches messages exclusively via ACP (Agent Client Protocol) — JSON-RPC over stdio to a co-located subprocess:

```
Discord/Slack msg ──► OpenAB ──stdio──► coding CLI (kiro, claude, codex…)
```

This means:
- **One agent per container.** The coding CLI binary must be bundled inside the same pod as OpenAB.
- **No parallelism across agents.** Running Claude Code *and* Kiro simultaneously requires deploying two full OpenAB stacks.
- **Pod-bound lifecycle.** If the pod restarts, the agent process (and any in-flight work) dies with it.
- **Resource coupling.** The agent shares CPU/memory/disk with OpenAB — a 90-minute refactor starves the broker.

AWS recently launched **Amazon Bedrock AgentCore Runtime**, which hosts coding agents (Kiro, Claude Code, Codex, etc.) in isolated Firecracker microVMs with persistent filesystems, session management, and streaming invoke APIs. This creates an opportunity: OpenAB can route messages to remote AgentCore sessions, decoupling the agent lifecycle from the broker.

### What this unlocks

1. **Dynamic multi-agent routing** — one OpenAB instance routes to N different AgentCore runtimes based on @mention or config.
2. **True isolation** — each agent runs in its own microVM; no shared localhost, no credential leakage.
3. **Background execution** — agents survive pod restarts, laptop lid closures, and network drops.
4. **Cost efficiency** — microVMs spin down when idle (pay per use), no always-on pod per agent.

---

## 2. Integration Approaches

Two viable approaches exist. We recommend **Option B**.

### Option A: OAB native SDK backend

Add a new `backend = "agentcore"` inside OAB that calls `InvokeAgentRuntime` directly via the AWS SDK.

```
Discord → OAB ──AWS SDK──► AgentCore Runtime (microVM)
```

### Option B: `agentcore-acp` adapter (recommended)

Write a standalone ACP-compatible adapter binary that bridges ACP stdio to AgentCore SDK calls. OAB treats it like any other coding CLI — zero OAB changes.

```
Discord → OAB ──ACP stdio──► agentcore-acp ──AWS SDK──► AgentCore Runtime (microVM)
```

### Why Option B wins

| Dimension | A. OAB native SDK | B. agentcore-acp adapter |
|-----------|-------------------|--------------------------|
| OAB code changes | Large — new trait, new backend, AWS SDK dep | **Zero** |
| Thin bridge philosophy | Violated — OAB learns AWS specifics | **Preserved** — OAB only speaks ACP |
| Onboarding pattern | New pattern for operators | **Same as kiro/claude/codex** |
| Independent dev/test | Coupled to OAB release cycle | **Standalone binary**, own repo/release |
| Language flexibility | Must be Rust (inside OAB) | **Any language** — Python PoC in hours |
| Multi-runtime routing | Requires OAB routing logic | Multiple OAB instances or adapter-level routing |
| Deployment | OAB pod needs IRSA for AgentCore | Adapter subprocess needs IRSA (same pod, same SA) |
| Streaming fidelity | Direct event-stream consumption | Adapter translates to ACP notifications (tiny overhead) |

Option A remains viable for future consideration if we find the ACP translation layer adds unacceptable latency or loses information. But given that every other agent integration (kiro-cli, claude-agent-acp, codex --acp, gemini --acp, opencode acp) follows the adapter pattern, Option B is the natural extension.

---

## 3. Design: `agentcore-acp`

### Architecture

```
┌─ agentcore-acp (subprocess, started by OAB) ─────────────────────┐
│                                                                   │
│  stdin ◄── ACP JSON-RPC from OAB                                  │
│  stdout ──► ACP JSON-RPC notifications to OAB                     │
│                                                                   │
│  ┌─────────────────────────────────────────────────────────────┐  │
│  │  ACP Server Layer                                           │  │
│  │  - session/new → create/resume AgentCore session            │  │
│  │  - session/prompt → InvokeAgentRuntime (streaming)          │  │
│  │  - cancel → StopRuntimeSession (best-effort)               │  │
│  └──────────────────────┬──────────────────────────────────────┘  │
│                         │                                         │
│  ┌──────────────────────▼──────────────────────────────────────┐  │
│  │  AgentCore Client                                           │  │
│  │  - boto3 / aws-sdk-rust / JS SDK                            │  │
│  │  - invoke_agent_runtime(runtimeArn, sessionId, payload)     │  │
│  │  - Stream text/event-stream → ACP content notifications     │  │
│  └─────────────────────────────────────────────────────────────┘  │
│                                                                   │
└───────────────────────────────────────────────────────────────────┘
                              │
                              ▼
                ┌──────────────────────────┐
                │  AgentCore Runtime (AWS)  │
                │  ┌────────────────────┐  │
                │  │ microVM            │  │
                │  │ Kiro / Claude /    │  │
                │  │ Codex / etc.       │  │
                │  │ /mnt/workspace     │  │
                │  └────────────────────┘  │
                └──────────────────────────┘
```

### ACP Protocol Mapping

| ACP Method (from OAB) | agentcore-acp Action |
|------------------------|---------------------|
| `session/new` | Generate `runtimeSessionId` from thread key, return session_id |
| `session/prompt` | `invoke_agent_runtime(payload={"prompt": text})` → stream response → emit ACP `notifications/content` blocks on stdout |
| `session/load` | Resume with same `runtimeSessionId` (AgentCore auto-mounts filesystem) |
| `cancel` | Best-effort: no direct cancel API; can `stop_runtime_session` if needed |

### Streaming Translation

AgentCore returns `text/event-stream`:
```
data: I'll analyze the code...
data: The issue is in line 42...
data: Here's my fix:
```

`agentcore-acp` translates each chunk to ACP JSON-RPC notification:
```json
{"jsonrpc":"2.0","method":"notifications/content","params":{"type":"text","text":"I'll analyze the code..."}}
{"jsonrpc":"2.0","method":"notifications/content","params":{"type":"text","text":"The issue is in line 42..."}}
```

This is the same format OAB already consumes from kiro-cli, claude-agent-acp, etc. — zero changes needed in OAB's streaming/edit logic.

### Session ID Mapping

AgentCore requires `runtimeSessionId` ≥ 33 characters. The adapter builds this deterministically from the ACP session context:

```
ACP session for Discord thread 1514294613853208667
  → runtimeSessionId = "oab-discord-1514294613853208667"  (34 chars ✓)

ACP session for Slack thread C0123456789.1234567890.123456
  → runtimeSessionId = "oab-slack-C0123456789-1234567890-123456"  (43 chars ✓)
```

Deterministic mapping means:
- No persistent state file needed in the adapter
- Resume works automatically after adapter restart
- Multiple adapter instances can share the same AgentCore sessions

---

## 4. Configuration

From the OAB operator's perspective, `agentcore-acp` is just another agent command:

```toml
[agent]
command = "agentcore-acp"
args = ["--runtime-arn", "arn:aws:bedrock-agentcore:us-west-2:123456789012:runtime/kiro-agent", "--region", "us-west-2"]
working_dir = "/home/agent"
# IAM credentials come from pod's service account (IRSA) — no env vars needed
```

For multi-agent setups, deploy multiple OAB instances each pointing to a different runtime:

```toml
# Instance 1: Kiro
[agent]
command = "agentcore-acp"
args = ["--runtime-arn", "arn:aws:...:runtime/kiro-agent"]

# Instance 2: Claude Code
[agent]
command = "agentcore-acp"
args = ["--runtime-arn", "arn:aws:...:runtime/claude-agent"]
```

Or, if the adapter supports it, a single adapter could route based on hints in the prompt/sender context (future enhancement).

---

## 5. AgentCore Runtime Characteristics

Key properties that the adapter must handle:

| Property | Value | Implication |
|----------|-------|-------------|
| Session idle timeout | 15 min (configurable, up to 8hr) | Each invoke resets the timer; long gaps = cold start |
| Max session lifetime | 8 hours | Long-running work needs session rotation |
| Cold start | ~5-15s (microVM boot) | First invoke per session has visible latency |
| Filesystem persistence | 14 days after last use | Agent state survives across session restarts |
| Streaming response | `text/event-stream` over HTTP/2 | Real-time token delivery, translatable to ACP |
| Parallelism | Independent microVM per session | No resource contention between sessions |
| Cost model | CPU-seconds + peak memory | Idle sessions cost nothing after termination |

### Comparison with local ACP

| Concern | ACP (local subprocess) | agentcore-acp (remote) |
|---------|----------------------|------------------------|
| Agent location | Same container | Remote microVM |
| Startup | Already running | Cold start on first invoke (~5-15s) |
| Session state | In-memory (process) | Persistent filesystem (/mnt/workspace) |
| Credential isolation | Shared pod env | Fully isolated (IAM + AgentCore Gateway) |
| Tool permission prompt | Supported (mid-turn) | Not supported — agents run autonomously |
| Max session duration | Unlimited (until pod dies) | 8 hours (configurable) |
| Resume after restart | Lost (unless session/save) | Automatic (filesystem persists) |
| Parallelism | Shared CPU per pod | One microVM per session |

---

## 6. Implementation Plan

### Phase 1: Python PoC

Minimal `agentcore-acp` in Python (fastest path to validation):

1. ACP stdio server (read JSON-RPC from stdin, write to stdout)
2. `session/new` → generate runtimeSessionId
3. `session/prompt` → `boto3.client('bedrock-agentcore').invoke_agent_runtime()` with streaming
4. Parse `response["response"].iter_lines()` → emit ACP content notifications
5. Package as a single script or small pip package

**Deliverable:** Working end-to-end demo: Discord message → OAB → agentcore-acp → AgentCore → streaming reply in Discord.

### Phase 2: Production hardening

1. Proper error handling (throttling, session terminated, cold start detection)
2. Cold start UX: emit a "⏳ Starting agent environment..." notification before streaming begins
3. Session resume logic (detect if session was idle-terminated, re-invoke transparently)
4. Config file support (runtime ARN, region, payload template, timeout)
5. Logging and observability (structured logs, latency metrics)

### Phase 3: Advanced features

1. Multi-runtime routing within a single adapter instance
2. `InvokeAgentRuntimeCommand` support for deterministic operations (exposed as an ACP tool?)
3. Rust rewrite for performance/single-binary distribution (if Python overhead is measurable)
4. Integration with AgentCore Gateway MCP for tool access

---

## 7. Open Questions

1. **Payload format** — Different AgentCore runtimes may expect different payload schemas (`{"prompt": "..."}` vs raw text vs MCP). Do we need a `--payload-template` flag?

2. **Session context passthrough** — Should the adapter forward OAB's sender context (user name, channel, etc.) in the payload so the remote agent knows who's asking?

3. **Human-in-the-loop** — ACP supports mid-turn tool permission prompts. AgentCore agents run autonomously. Is this acceptable, or do we need a callback mechanism via the adapter?

4. **Cold start notification** — How should the adapter signal to OAB that the agent is booting? Options: immediate ACP notification ("Starting environment..."), or let OAB's existing stall detection handle it.

5. **Cancel semantics** — AgentCore has `StopRuntimeSession` but no mid-invoke cancel. Should `cancel` kill the entire session (losing state), or just be a no-op?

6. **Multi-agent routing** — Single adapter routing to multiple runtimes, or multiple adapter instances? Former is more convenient, latter is simpler.

7. **Language choice for production** — Python (fast to write, boto3 native), Rust (single binary, matches OAB ecosystem), or Node.js (middle ground)?

---

## 8. Alternatives Considered

### A. OAB native SDK backend (not recommended for now)

Add `backend = "agentcore"` directly inside OAB with AWS SDK calls. This works but:
- Violates the "thin bridge" philosophy — OAB shouldn't understand AWS specifics
- Adds `aws-sdk-bedrockagentcore` as a compile-time dependency to OAB
- Different release cycle (AgentCore API changes shouldn't require OAB rebuild)
- Breaks the consistent "all agents are ACP subprocesses" mental model

May revisit if the ACP translation layer proves to be a bottleneck.

### B. Deploy OAB itself on AgentCore

Run the entire OpenAB + agent container on AgentCore Runtime. This works but:
- Still couples agent to container
- Doesn't leverage AgentCore's multi-session isolation
- Doesn't enable dynamic routing from one OAB instance
- Loses the thin bridge role

### C. WebSocket relay to AgentCore

Persistent WebSocket between OAB and a custom proxy. Rejected:
- Adds another service to deploy
- `InvokeAgentRuntime` already streams; no intermediary needed
- More moving parts, same result

### D. MCP-based integration via AgentCore Gateway

Use Gateway's MCP endpoint as a tool layer for local agents. Complementary (could add for tools) but doesn't solve the agent lifecycle coupling problem.

---

## 9. References

- [AWS Blog: Hosting Coding Agents on AgentCore](https://aws.amazon.com/blogs/machine-learning/its-safe-to-close-your-laptop-now-hosting-coding-agents-on-amazon-bedrock-agentcore/)
- [InvokeAgentRuntime API Reference](https://docs.aws.amazon.com/bedrock-agentcore/latest/APIReference/API_InvokeAgentRuntime.html)
- [InvokeAgentRuntimeCommand API Reference](https://docs.aws.amazon.com/bedrock-agentcore/latest/APIReference/API_InvokeAgentRuntimeCommand.html)
- [AgentCore Runtime Lifecycle Settings](https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/runtime-lifecycle-settings.html)
- [AgentCore Session Storage (Preview)](https://aws.amazon.com/about-aws/whats-new/2026/03/bedrock-agentcore-runtime-session-storage/)
- [Handle Long-Running Agents](https://docs.aws.amazon.com/bedrock-agentcore/latest/devguide/runtime-long-run.html)
- [OpenAB DESIGN.md](../../DESIGN.md) — "Thin Bridge" philosophy
- [ADR: openab-agent](./openab-agent.md) — Native agent pattern (similar standalone approach)

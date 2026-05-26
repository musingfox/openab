# ADR: openab-agent — Native Rust Coding Agent with Built-in ACP

- **Status:** Proposed
- **Date:** 2026-05-26
- **Author:** @chaodu-agent

---

## 1. Context & Motivation

Today, every coding agent in OpenAB follows the same pattern:

```
openab (Rust) ──stdio JSON-RPC──► adapter (Node/Rust) ──spawns──► CLI agent ──HTTP──► LLM API
```

This introduces 3–4 layers of indirection, each with its own runtime, dependencies, and failure modes. Every agent requires:

- A separate Dockerfile (300–800MB images)
- A Node.js or Python runtime
- An ACP adapter wrapper (pi-acp, codex-acp, agy-acp, etc.)
- npm/pip supply-chain management

Meanwhile, the actual work an agent does is simple:

1. Receive a prompt
2. Call an LLM API (HTTP POST + SSE)
3. Execute tool calls (read/write/edit/bash)
4. Return the result

**Proposal:** Build `openab-agent` — a single Rust binary that is both the ACP server and the coding agent, with no external runtime, no wrapper, and no adapter layer.

---

## 2. Design

### Architecture

```
┌─ openab-agent (single Rust binary) ──────────────┐
│                                                   │
│  ┌─────────────────────────────────────────────┐  │
│  │  ACP Layer (stdin/stdout JSON-RPC)          │  │
│  │  - session/new, session/prompt, cancel      │  │
│  └──────────────────┬──────────────────────────┘  │
│                     │                             │
│  ┌──────────────────▼──────────────────────────┐  │
│  │  Agent Core                                 │  │
│  │  - Prompt assembly (system + user + tools)  │  │
│  │  - Tool dispatch loop                       │  │
│  │  - Session tree (branching history)         │  │
│  └──────────────────┬──────────────────────────┘  │
│                     │                             │
│  ┌──────────────────▼──────────────────────────┐  │
│  │  LLM Client (reqwest + SSE)                 │  │
│  │  - OpenAI-compatible (GPT, Codex, DeepSeek) │  │
│  │  - Anthropic (Claude)                       │  │
│  │  - Google (Gemini)                          │  │
│  └─────────────────────────────────────────────┘  │
│                                                   │
│  ┌─────────────────────────────────────────────┐  │
│  │  Tools (4 only)                             │  │
│  │  - read: file/directory reading             │  │
│  │  - write: file creation                     │  │
│  │  - edit: string replacement in files        │  │
│  │  - bash: command execution (sandboxed)      │  │
│  └─────────────────────────────────────────────┘  │
│                                                   │
└───────────────────────────────────────────────────┘
```

### Comparison with Existing Agents

| Aspect | Existing (e.g. Pi, Codex) | openab-agent |
|--------|---------------------------|--------------|
| Layers | openab → adapter → CLI → LLM | openab → **agent** → LLM |
| Runtime | Node.js / Python | None (static binary) |
| Image size | 300–800MB | ~20MB (distroless) |
| Cold start | 1–3s | <50ms |
| ACP support | Requires wrapper | Native, zero overhead |
| Dependencies | npm ecosystem | Minimal crates |
| Supply-chain risk | High (node_modules) | Low (cargo audit) |

### Design Principles (inspired by Pi)

1. **Minimal tool surface** — 4 tools only (read, write, edit, bash). Maximizes context window for actual code.
2. **Tiny system prompt** — Agent instructions fit in ~500 tokens. No bloated tool descriptions.
3. **Multi-model** — Provider-agnostic. Switch models via config or mid-session command.
4. **Session trees** — Branching conversation history. Explore multiple approaches without losing context.
5. **No SDK dependency** — LLM APIs are just HTTP. A thin `reqwest` client (~300 lines) covers all providers.

---

## 3. LLM Client Design

The LLM client is intentionally thin — no SDK, just HTTP:

```rust
// Unified trait for all providers
trait LlmProvider {
    async fn chat(&self, messages: &[Message], tools: &[Tool]) -> Result<Stream<Event>>;
}

// Implementations are ~100 lines each
struct OpenAiProvider { base_url: String, api_key: String, model: String }
struct AnthropicProvider { api_key: String, model: String }
struct GoogleProvider { api_key: String, model: String }
```

All three major APIs follow the same pattern:
- POST JSON body with messages + tool definitions
- Stream SSE events back
- Parse tool_use / function_call blocks
- Loop until stop

Provider differences are minor (header format, JSON schema for tools, SSE event names) and well-contained in ~300 lines per provider.

### API Change Tracking

Without an official SDK, API changes must be tracked manually. Mitigation:

- Pin to stable API versions (`anthropic-version: 2023-06-01`)
- CI test against each provider's API weekly (canary job)
- OpenAI-compatible endpoint covers most providers (DeepSeek, Groq, Together, etc.) with zero additional code

---

## 4. Tool Implementation

| Tool | Input | Behavior |
|------|-------|----------|
| `read` | path, optional line range | Read file contents or list directory |
| `write` | path, content | Create or overwrite file |
| `edit` | path, old_str, new_str | Replace exact string in file |
| `bash` | command, optional working_dir | Execute shell command, return stdout/stderr |

### Sandboxing (bash tool)

The `bash` tool runs commands via `tokio::process::Command` with:

- Configurable timeout (default: 120s)
- Optional `bubblewrap` (bwrap) sandboxing for untrusted execution
- Working directory scoped to agent's `working_dir`
- No network access restriction by default (agent needs to call APIs, git, etc.)

---

## 5. Session Tree

Sessions are stored as a tree structure, not a flat list:

```
root
├── turn-1 (user: "explain this code")
│   └── turn-2 (assistant: "This code does...")
│       ├── turn-3a (user: "refactor it") ← branch A
│       │   └── turn-4a (assistant: "Here's the refactored...")
│       └── turn-3b (user: "add tests instead") ← branch B
│           └── turn-4b (assistant: "Here are the tests...")
```

Benefits:
- Explore multiple approaches from a decision point
- Rollback without losing the exploration
- Persisted to disk as JSON for session resume

---

## 6. Configuration

```toml
[agent]
command = "openab-agent"
working_dir = "/home/agent"

[agent.env]
# Provider selection (one of):
OPENAB_AGENT_PROVIDER = "anthropic"          # or "openai", "google", "openai-compatible"
OPENAB_AGENT_MODEL = "claude-sonnet-4-20250514"

# Auth (provider-specific):
ANTHROPIC_API_KEY = "${ANTHROPIC_API_KEY}"
# or: OPENAI_API_KEY, GOOGLE_API_KEY, etc.

# Optional:
OPENAB_AGENT_MAX_TOKENS = "8192"
OPENAB_AGENT_TIMEOUT_SECS = "120"
```

### Steering Files

openab-agent reads steering files in the same pattern as other agents:

- `AGENTS.md` in working directory (hot memory, always loaded)
- `.openab-agent/system.md` — custom system prompt override
- `.openab-agent/append.md` — append to default system prompt

---

## 7. Future: Library Mode

Because openab-agent is Rust (same as openab core), a future optimization is **in-process mode** — no stdio, no JSON-RPC serialization:

```rust
// Current: IPC over stdio
openab::spawn_process("openab-agent", &["--acp"]) // stdin/stdout JSON-RPC

// Future: direct function call
let agent = openab_agent::Agent::new(config);
let response = agent.prompt(session_id, messages).await; // zero-copy
```

This eliminates all IPC overhead. Not in scope for v1, but the architecture enables it.

---

## 8. Rollout Plan

| Phase | Scope | Deliverable |
|-------|-------|-------------|
| **v0.1** | Scaffold + ACP layer + single provider (Anthropic) | Working agent, 4 tools, flat session |
| **v0.2** | Multi-provider + session tree + steering files | Feature parity with Pi's core |
| **v0.3** | Dockerfile + Helm chart + CI | Production-ready deployment |
| **v0.4** | Library mode exploration | In-process integration with openab core |

---

## 9. Open Questions

| Question | Options | Notes |
|----------|---------|-------|
| **Crate name** | `openab-agent` as a workspace member vs separate repo | Workspace member keeps it close to openab core |
| **Subscription auth** | Support OAuth flows (Claude Pro, ChatGPT Plus) or API-key only for v1? | API-key only for v1; subscription auth adds complexity |
| **Permission model** | Auto-approve all tool calls vs interactive approval? | Auto-approve for v1 (matches OpenAB's `--trust-all-tools` pattern) |
| **Context window management** | Truncate old turns vs summarize vs session tree branching? | Session tree branching for v1; summarization for v2 |

---

## Consequences

### Positive

- **Zero external runtime** — no Node.js, Python, or npm. Single static binary.
- **Minimal attack surface** — no node_modules supply chain, no adapter layer vulnerabilities.
- **Fastest cold start** — <50ms vs 1–3s for Node-based agents.
- **Smallest image** — ~20MB distroless vs 300–800MB for existing agents.
- **Native ACP** — no wrapper overhead, no adapter bugs, no version mismatches.
- **Same language as openab** — shared types, potential library mode, unified toolchain.
- **Full control** — no upstream CLI breaking changes; we own the entire stack.

### Negative

- **LLM API maintenance** — must track API changes manually without official SDKs.
- **No subscription auth (v1)** — API key only initially; users with Claude Pro/ChatGPT Plus subscriptions still need Pi or Codex.
- **Feature gap** — v1 will lack features mature agents have (image support, MCP, web search tools).
- **Development effort** — building from scratch vs leveraging existing open-source agents.

### Risks

- **API instability** — if providers make breaking changes frequently, maintenance burden grows. Mitigated by pinning API versions and weekly CI canary.
- **Scope creep** — temptation to add more tools/features. Mitigated by the "4 tools only" design principle as a hard constraint for v1.

---

## References

- [Pi coding agent](https://github.com/earendil-works/pi) — design inspiration (minimal tools, session trees, multi-model)
- [Agent Client Protocol](https://github.com/anthropics/agent-protocol) — ACP spec
- [OpenAB](https://github.com/openabdev/openab) — host runtime

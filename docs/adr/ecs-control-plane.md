# ADR: ECS Control Plane (CRD + Operator Pattern on ECS)

- **Status:** Proposed
- **Date:** 2026-05-18
- **Author:** @pahud.hsieh
- **Related:** [Multi-Platform Adapters](./multi-platform-adapters.md), [Basic CronJob](./basic-cronjob.md)

---

## 1. Context & Motivation

OpenAB currently deploys on Kubernetes using Helm charts. While K8s provides a mature operator pattern (CRD + Controller), many teams prefer or require **Amazon ECS** for its operational simplicity and tighter AWS integration.

We want to bring the same declarative, self-healing deployment model to ECS:

- Operators declare desired state in YAML manifests (analogous to CRDs)
- A controller reconciles desired state against actual ECS resources
- OAB instances + arbitrary backends are deployed and maintained automatically

This enables a "GitOps for ECS" workflow where pushing a YAML change triggers the controller to converge the cluster to the new desired state.

---

## 2. Design Overview

```
┌──────────────────────────────────────────────────────┐
│  ECS Control Plane (runs as ECS Service)             │
│                                                      │
│  ┌────────────┐  ┌──────────────┐  ┌─────────────┐  │
│  │ State Store│  │  Reconciler  │  │  ECS API /  │  │
│  │   (S3)     │◄─│  Controller  │─►│  CloudMap   │  │
│  │            │  │              │  │             │  │
│  └────────────┘  └──────────────┘  └─────────────┘  │
│                        ▲                             │
│                        │ events / poll               │
│                        ▼                             │
│                 ┌──────────────┐                     │
│                 │ S3 Events /  │                     │
│                 │ EventBridge  │                     │
│                 └──────────────┘                     │
└──────────────────────────────────────────────────────┘
```

### Core Loop (Reconciliation)

1. Load all YAML manifests from S3 (desired state)
2. Describe current ECS services/tasks (observed state)
3. Compute diff (compare `metadata.generation` vs `status.observedGeneration`)
4. Apply changes: create, update, or delete ECS resources
5. Write status back to S3 (separate prefix), set `status.observedGeneration = metadata.generation`
6. Sleep / wait for next event

---

## 3. Manifest Schema

### API Identity (fixed)

| Field | Value |
|-------|-------|
| `apiVersion` | `oab.dev/v1` |
| `kind` | `OABService` |

All examples in this ADR use this identity. No other combinations (`openab.dev/v1`, `AgentDeployment`) are valid.

### Full Example

```yaml
apiVersion: oab.dev/v1
kind: OABService
metadata:
  name: my-agent
  namespace: prod
  generation: 4                    # incremented by oabctl on each apply
spec:
  replicas: 1
  capacityProvider: FARGATE        # FARGATE (default) or FARGATE_SPOT
  cpu: 256                         # vCPU units (256 = 0.25 vCPU)
  memory: 512                      # MB
  taskDefinition:
    image: 123456789.dkr.ecr.us-east-1.amazonaws.com/openab:latest
  bootstrapFrom: s3://oab-state/agents/my-agent/latest.tar.gz
  secrets:
    - name: DISCORD_BOT_TOKEN
      source: ssm
      path: /oab/my-agent/discord-token
    - name: LLM_API_KEY
      source: secretsmanager
      arn: arn:aws:secretsmanager:us-east-1:123:secret:oab/my-agent/llm-key
  networking:
    subnets: [subnet-abc, subnet-def]
    securityGroups: [sg-123]
    assignPublicIp: false
  config:
    agent:
      name: my-agent
      backend: bedrock
      model: us.anthropic.claude-sonnet-4-20250514
    discord:
      enabled: true
      botId: "123456789"
      guildId: "987654321"
      channelIds: ["111111111"]
    steering:
      source: s3
      bucket: oab-steering
      prefix: agents/my-agent/
    memory:
      backend: s3
      bucket: oab-memory
      prefix: agents/my-agent/
    tools:
      github: { enabled: true }
      web: { enabled: true }
status:
  phase: Running                   # Pending | Running | Failed | Terminating
  observedGeneration: 4            # last generation the controller reconciled
  taskArns:
    - arn:aws:ecs:us-east-1:123456789012:task/cluster/abc123
  lastReconciled: "2026-05-18T22:50:00Z"
  conditions:
    - type: Available
      status: "True"
      lastTransitionTime: "2026-05-18T22:50:00Z"
```

### Key Fields

| Field | Description |
|-------|-------------|
| `metadata.generation` | Monotonically increasing counter, bumped by `oabctl apply` |
| `spec.capacityProvider` | `FARGATE` (on-demand) or `FARGATE_SPOT` (up to 70% savings, tolerates interruption) |
| `spec.cpu` / `spec.memory` | Maps to ECS task definition (must be valid Fargate combination) |
| `spec.taskDefinition.image` | Container image |
| `spec.bootstrapFrom` | S3 path to mutable state archive (memory, knowledge base — **no secrets**) |
| `spec.secrets` | Per-agent secret references (SSM / Secrets Manager) |
| `spec.config` | Structured agent config; controller renders to `config.toml` |
| `spec.networking` | ECS awsvpc configuration |
| `status.observedGeneration` | Last generation the controller successfully reconciled |

### Replicas Semantics

OAB agents are **single-instance** by design — each agent holds one adapter connection (WebSocket gateway for Discord/Telegram/Slack). There is no load balancing across agent replicas.

**Rules:**
- `replicas: 1` — the only valid value
- Controller **rejects** `replicas > 1` at validation time
- Scaling is horizontal by deploying **more agents** (each with its own bot token), not by replicating one agent

### Fleet Provisioning (`OABFleet`)

Enterprise scenario: provision 10-20 agents in one apply. Controller handles everything including Discord Bot registration.

```yaml
apiVersion: oab.dev/v1
kind: OABFleet
metadata:
  name: enterprise-team
  namespace: prod
spec:
  defaults:
    capacityProvider: FARGATE_SPOT
    cpu: 512
    memory: 1024
    taskDefinition:
      image: ghcr.io/openabdev/openab:latest
    networking:
      subnets: [subnet-abc, subnet-def]
      securityGroups: [sg-oab]
    discord:
      autoRegister: true          # controller creates Bot via Discord API
  agents:
    - name: kiro-01
      config: { agent: { backend: kiro } }
    - name: kiro-02
      config: { agent: { backend: kiro } }
    - name: kiro-03
      config: { agent: { backend: kiro } }
    - name: codex-01
      config: { agent: { backend: codex } }
      cpu: 1024
      memory: 2048               # override defaults
    - name: codex-02
      config: { agent: { backend: codex } }
      cpu: 1024
      memory: 2048
    - name: gemini-01
      config: { agent: { backend: gemini } }
    - name: gemini-02
      config: { agent: { backend: gemini } }
    - name: gemini-03
      config: { agent: { backend: gemini } }
    - name: gemini-04
      config: { agent: { backend: gemini } }
    - name: gemini-05
      config: { agent: { backend: gemini } }
```

### Discord Auto-Registration Flow

When `discord.autoRegister: true`, the controller provisions Discord Bots automatically:

```
oabctl apply -f fleet.yaml
  │
  │  For each agent:
  ├─ 1. Discord API: POST /applications → create Bot Application
  ├─ 2. Discord API: POST /applications/{id}/bot → get Bot Token
  ├─ 3. Store token → SSM /oab/{namespace}/{name}/discord-token
  ├─ 4. Generate OAuth2 invite URL
  ├─ 5. Create ECS Service (desiredCount=1)
  └─ 6. Write status (phase=Running, inviteUrl=...)
```

**Apply output:**

```bash
$ oabctl apply -f fleet.yaml

✓ kiro-01   provisioned → https://discord.com/oauth2/authorize?client_id=AAA&scope=bot
✓ kiro-02   provisioned → https://discord.com/oauth2/authorize?client_id=BBB&scope=bot
✓ kiro-03   provisioned → https://discord.com/oauth2/authorize?client_id=CCC&scope=bot
✓ codex-01  provisioned → https://discord.com/oauth2/authorize?client_id=DDD&scope=bot
✓ codex-02  provisioned → https://discord.com/oauth2/authorize?client_id=EEE&scope=bot
✓ gemini-01 provisioned → https://discord.com/oauth2/authorize?client_id=FFF&scope=bot
✓ gemini-02 provisioned → https://discord.com/oauth2/authorize?client_id=GGG&scope=bot
✓ gemini-03 provisioned → https://discord.com/oauth2/authorize?client_id=HHH&scope=bot
✓ gemini-04 provisioned → https://discord.com/oauth2/authorize?client_id=III&scope=bot
✓ gemini-05 provisioned → https://discord.com/oauth2/authorize?client_id=JJJ&scope=bot

10 agents provisioned. Add them to your server using the URLs above.
```

**User's only manual step:** paste the OAuth URL into a browser → authorize the bot to join their Discord server.

### Responsibility Model

| Layer | Responsibility |
|-------|---------------|
| `oabctl` / Controller | Desired state: create Bots, store tokens, create ECS Services |
| ECS | Runtime health: task dies → auto-restart (desiredCount=1) |
| User | One-time: add bots to Discord server via OAuth URL |

The controller does **not** monitor agent health — ECS Service already maintains desired state. If a task crashes, ECS replaces it automatically. The controller only acts when the **desired state** (manifest) changes.

### Prerequisites for Auto-Registration

- Discord Developer Portal credentials stored in SSM: `/oab/discord-developer/token`
- Controller IAM role needs `ssm:PutParameter` to store generated bot tokens
- Discord API rate limit: ~5 app creations per minute (controller handles backoff)

---

## 4. Multi-Runtime Support (ECS + K8s)

The same YAML manifest can deploy to **both ECS and Kubernetes**. The spec is platform-agnostic; platform-specific details live in an optional `platform:` overlay.

### Design Principle

```
┌─────────────────────┐
│  oab.dev/v1 YAML    │  ← one spec, platform-agnostic core
└──────────┬──────────┘
           │
    ┌──────┴──────┐
    ▼             ▼
┌────────┐   ┌────────┐
│  ECS   │   │  K8s   │
│Controller│   │Operator│
└────┬───┘   └────┬───┘
     ▼             ▼
  ECS Service   Deployment + ConfigMap + ExternalSecret
```

Each controller reads the core spec and its own `platform:` overlay, ignoring the other.

### Spec with Platform Overlay

```yaml
apiVersion: oab.dev/v1
kind: OABService
metadata:
  name: chaodu
  namespace: prod
spec:
  # Core (cross-platform)
  cpu: 512
  memory: 1024
  config:
    agent: { backend: kiro }
    discord: { enabled: true, botId: "123" }
  secrets:
    - name: DISCORD_BOT_TOKEN
      source: ssm
      path: /oab/chaodu/discord-token

  # Platform-specific (each controller reads only its own key)
  platform:
    ecs:
      capacityProvider: FARGATE_SPOT
      networking:
        subnets: [subnet-abc, subnet-def]
        securityGroups: [sg-oab]
        assignPublicIp: false
    k8s:
      nodeSelector: { node.kubernetes.io/capacity-type: spot }
      serviceAccount: oab-agent
      storageClass: gp3
```

### Translation Table

| Core Spec | ECS Controller | K8s Operator |
|-----------|---------------|--------------|
| `cpu: 512` | TaskDef `cpu=512` | `resources.requests.cpu: 500m` |
| `memory: 1024` | TaskDef `memory=1024` | `resources.requests.memory: 1Gi` |
| `spec.config` | Render → S3 artifact → startup wrapper | Render → ConfigMap → volume mount |
| `spec.secrets[].source: ssm` | ECS native `secrets` field | ExternalSecret → K8s Secret |
| `platform.ecs.capacityProvider` | Fargate capacity provider | _(ignored)_ |
| `platform.k8s.nodeSelector` | _(ignored)_ | Pod nodeSelector |

### Rules

- `platform:` is optional. If omitted, controller uses its own defaults.
- Controller **ignores** unknown platform keys (ECS controller skips `platform.k8s`, and vice versa).
- Core spec fields (`cpu`, `memory`, `config`, `secrets`) are mandatory and cross-platform.
- `OABFleet` also supports `platform:` at both `defaults` and per-agent level.

### Phase Plan

- **Phase 1**: ECS controller only. `platform.ecs` supported, `platform.k8s` ignored.
- **Phase 3**: K8s operator reads same manifests (from S3 or as native CRD). Shared schema, different runtime.

---

## 5. Config Delivery Model

The controller does **not** mount config into containers (ECS/Fargate has no shared volume equivalent to K8s ConfigMap). Instead:

### Flow

```
oabctl apply -f agent.yaml
  → writes manifest to S3 (manifests/{ns}/{name}.yaml)

Controller reconcile:
  → reads spec.config from manifest
  → renders config.toml
  → writes to s3://oab-control-plane/artifacts/{ns}/{name}/config.toml
  → registers new ECS TaskDefinition (or forces new deployment)

ECS Task startup (entrypoint wrapper):
  → s3:GetObject artifacts/{ns}/{name}/config.toml → /home/agent/config.toml
  → s3:GetObject ${bootstrapFrom} → tar xzf → /home/agent/ (mutable state only)
  → exec openab
```

### Entrypoint Wrapper

```bash
#!/bin/bash
set -e
# Download controller-rendered config
aws s3 cp "s3://oab-control-plane/artifacts/${NAMESPACE}/${NAME}/config.toml" /home/agent/config.toml
# Restore mutable state (memory, knowledge base) if bootstrapFrom is set
if [ -n "$BOOTSTRAP_FROM" ]; then
  aws s3 cp "$BOOTSTRAP_FROM" /tmp/bootstrap.tar.gz
  tar xzf /tmp/bootstrap.tar.gz -C /home/agent/
  rm /tmp/bootstrap.tar.gz
fi
exec /usr/local/bin/openab
```

### What Goes Where

| Content | Location | Managed By |
|---------|----------|------------|
| `config.toml` | S3 artifact (controller renders) | `spec.config` in manifest |
| Secrets (bot tokens, API keys) | SSM / Secrets Manager | `spec.secrets` in manifest |
| Memory / knowledge base | `bootstrapFrom` archive | `oabctl snapshot` |
| Steering files | S3 (referenced in config.toml) | Separate steering bucket |

**Secrets never go in the bootstrap archive.** The archive contains only mutable runtime state that the agent accumulates over time.

---

## 6. Per-Agent Secret Injection

Each agent/bot has its **own** credentials — no token sharing between agents.

### Design Principles

- Each `OABService` owns its secrets (1:1 mapping)
- Controller never touches secret values — it only wires references into ECS Task Definitions
- ECS native `secrets` field handles injection at runtime
- IAM scoping ensures each task role can only read its own secret path

### Spec

```yaml
spec:
  secrets:
    - name: DISCORD_BOT_TOKEN
      source: ssm
      path: /oab/chaodu/discord-token
    - name: LLM_API_KEY
      source: secretsmanager
      arn: arn:aws:secretsmanager:us-east-1:123:secret:oab/chaodu/llm-key
```

### Controller Behavior

1. **Deploy** — maps `spec.secrets` to ECS TaskDefinition `secrets` field:
   ```json
   {
     "secrets": [
       { "name": "DISCORD_BOT_TOKEN", "valueFrom": "/oab/chaodu/discord-token" },
       { "name": "LLM_API_KEY", "valueFrom": "arn:aws:secretsmanager:us-east-1:123:secret:oab/chaodu/llm-key" }
     ]
   }
   ```
2. **IAM** — task execution role scoped to the agent's secret path:
   ```json
   {
     "Effect": "Allow",
     "Action": ["ssm:GetParameters", "secretsmanager:GetSecretValue"],
     "Resource": [
       "arn:aws:ssm:*:*:parameter/oab/chaodu/*",
       "arn:aws:secretsmanager:*:*:secret:oab/chaodu/*"
     ]
   }
   ```

### Secret Rotation Lifecycle

```
1. Operator rotates secret in SSM/Secrets Manager (manual or auto-rotation)
2. Controller detects rotation:
   - Option A: spec.secrets[].autoRestart: true → controller forces new deployment
   - Option B: operator runs `oabctl restart my-agent`
3. ECS launches new task → new task fetches fresh secret value at startup
4. Old task drains and stops (ECS rolling update)
5. Controller updates status:
   - conditions[].type: SecretsRefreshed
   - conditions[].lastTransitionTime: <now>
```

**Failure handling:**
- If new task fails to start (bad secret value), ECS circuit breaker stops the rollout
- Controller sets `status.phase: Failed`, `conditions[].type: SecretInjectionFailed`
- Old task remains running (ECS deployment circuit breaker preserves last healthy state)

---

## 7. State Store Design (S3-Only)

```
s3://oab-control-plane/
  ├── manifests/{namespace}/{name}.yaml     ← desired state (oabctl writes)
  ├── status/{namespace}/{name}.json        ← observed state (controller writes)
  └── artifacts/{namespace}/{name}/         ← rendered config.toml (controller writes)
```

| Concern | Mechanism | Rationale |
|---------|-----------|-----------|
| Desired state | `manifests/` prefix | Human-readable, git-syncable, versioned via S3 versioning |
| Status | `status/` prefix | Controller writes after each reconcile cycle |
| Config artifacts | `artifacts/` prefix | Controller-rendered config.toml for task startup |
| Generation tracking | `metadata.generation` in manifest YAML | Explicit counter, not tied to S3 VersionId |
| Change detection | S3 Event Notifications → EventBridge (Phase 2) | Phase 1 uses polling |
| Consistency | S3 strong read-after-write | Sufficient for single-controller |
| Optimistic locking | S3 conditional writes (If-None-Match / ETag) | Prevents concurrent `oabctl apply` conflicts |

### Generation vs S3 VersionId

S3 VersionId is an opaque string — not suitable for comparing "which is newer." Instead:
- `metadata.generation` is an explicit integer, incremented by `oabctl apply`
- `status.observedGeneration` records the last generation the controller reconciled
- Controller skips reconcile if `observedGeneration == generation` (no-op)
- Stale status writes are detected: if status.observedGeneration < manifest.generation, the status is outdated

### Delete Semantics (Phase 2)

Phase 1: `oabctl delete` removes the manifest from S3; controller detects absence and tears down ECS resources.

Phase 2: Proper deletion with finalizers:
1. `oabctl delete` sets `metadata.deletionTimestamp` in the manifest (tombstone)
2. Controller runs finalizers (drain connections, cleanup CloudMap, remove artifacts)
3. Controller removes manifest and status objects only after all finalizers complete

---

## 8. Controller Upgrade Strategy

The controller runs as a single-replica ECS Service.

### Phase 1 (acceptable brief gap)

```yaml
# Controller's own ECS Service config
deploymentConfiguration:
  minimumHealthyPercent: 0      # allow old to stop before new starts
  maximumPercent: 100
```

- ECS rolling update: stop old → start new
- Brief reconciliation gap (30-60s) during upgrade
- No in-flight reconcile is lost — next cycle picks up any drift
- Acceptable for Phase 1 because reconcile is idempotent

### Phase 2 (zero-downtime)

- DynamoDB-based leader election (two controller replicas)
- Active/standby: standby takes over within seconds if active fails health check
- Version skew handling: new controller must handle manifests written by old `oabctl` versions (schema backward compatibility)

### Rollback

- Controller image is pinned in its own ECS TaskDefinition
- Rollback = `aws ecs update-service --task-definition <previous-revision>`
- Controller state is in S3 (stateless process), so rollback is safe

---

## 9. CLI UX (`oabctl`)

### Core Commands

```bash
oabctl apply -f agent.yaml          # declare/update desired state
oabctl get oabservice               # list all services + status
oabctl get oabservice my-agent      # single service detail
oabctl delete oabservice my-agent   # remove (Phase 1: immediate; Phase 2: finalizer)
oabctl diff -f agent.yaml           # show local vs remote diff
oabctl logs my-agent                # shortcut to ECS task logs (CloudWatch)
oabctl restart my-agent             # force new deployment (pick up rotated secrets)
oabctl snapshot my-agent            # capture runtime state → bootstrapFrom archive
oabctl wait my-agent --for=Available # block until condition met
```

### `apply` Semantics

```
$ oabctl apply -f prod/my-agent.yaml

✓ Schema validated (oab.dev/v1 OABService)
✓ Replicas check passed (replicas=1)
✓ Uploaded to s3://oab-control-plane/manifests/prod/my-agent.yaml
✓ Generation: 3 → 4
⏳ Waiting for reconciliation...
✓ Service my-agent reconciled (observedGeneration=4, 1/1 tasks running)
```

### `diff` Granularity

```bash
oabctl diff -f agent.yaml              # spec-only: local YAML vs remote manifest
oabctl diff -f agent.yaml --rendered   # rendered: show generated config.toml diff
oabctl diff -f agent.yaml --status     # include status comparison
```

### Implementation

`oabctl` talks directly to S3 via AWS SDK. No API server needed. Auth is standard IAM (role, profile, env vars). Config stored in `~/.oabctl/config`:

```toml
[default]
region = "us-east-1"
bucket = "oab-control-plane"
cluster = "oab-prod"
```

---

## 10. Phase Scope

### Phase 1 — MVP (target)

| In Scope | Out of Scope |
|----------|--------------|
| S3 manifest store (versioning enabled) | EventBridge triggers |
| Single-instance controller (poll every 30s) | Multi-replica controller / leader election |
| `oabctl apply` / `oabctl get` | `oabctl delete` with finalizers |
| Controller renders config.toml → S3 artifact | DynamoDB state store |
| ECS service create / update | Rollback (`oabctl rollback`) |
| Startup wrapper downloads config + bootstrap | EFS / shared volumes |
| `metadata.generation` / `status.observedGeneration` | Multi-region |
| Per-agent secrets via SSM/SM | Auto-rotation detection |
| Replicas validation (reject >1) | Auto-scaling policies |

### Phase 2

- Event-driven triggers (S3 → EventBridge → controller)
- `oabctl delete` with tombstone + finalizers
- `oabctl diff`, `oabctl logs`, `oabctl restart`
- DynamoDB for leader election (active/standby controller)
- Secret auto-rotation detection + auto-restart
- Rollback via generation history

### Phase 3

- **K8s Operator** — same `oab.dev/v1` schema consumed as native CRD; `platform.k8s` overlay
- Multi-region (controller per region, S3 cross-region replication)
- Dependency graph (service A depends on service B)
- Auto-scaling policies in manifest spec
- GitOps integration (GitHub Actions → `oabctl apply` on push)
- Schema versioning + migration tooling

---

## 11. Alternatives Considered

| Alternative | Why not chosen |
|-------------|---------------|
| AWS Proton | Opinionated, limited customization for OAB-specific logic |
| AWS Copilot | Good for simple apps, no custom reconciliation loop |
| CDK Pipelines | Deployment tool, not a runtime controller with drift detection |
| Step Functions orchestrator | Stateless execution model, no continuous reconciliation |
| Run K8s anyway (EKS) | Valid but adds operational overhead for teams that chose ECS |
| DynamoDB as primary store | Adds infra; S3 sufficient for single-controller Phase 1 |

---

## 12. Open Questions

1. **Multi-region** — single controller per region, or global controller with regional reconcilers?
2. **Observability** — CloudWatch metrics from the controller, or push to a shared OAB dashboard?
3. **Networking isolation** — shared VPC with per-service SG rules, or per-namespace VPC?
4. **Schema versioning** — how to handle `oab.dev/v2` migration when spec evolves?

---

## 13. Decision

We adopt the CRD + Operator pattern on ECS with an **S3-only state store**, **explicit generation tracking**, and a **`oabctl` CLI** for the operator interface. The controller runs as a single ECS service that reconciles `OABService` manifests against actual ECS state.

Key design choices:
- **Config delivery**: controller renders `config.toml` to S3 artifact; startup wrapper downloads it
- **Secrets**: per-agent SSM/Secrets Manager references; never in bootstrap archive
- **Bootstrap**: mutable runtime state only (memory, knowledge base)
- **Replicas**: always 1; scale by deploying more agents, not replicating one
- **Generation**: explicit `metadata.generation` / `status.observedGeneration` (not S3 VersionId)
- **Phase 1 scope**: narrow (create/update only, poll-based, single controller)

DynamoDB, EventBridge, finalizers, and multi-region are deferred to Phase 2+.

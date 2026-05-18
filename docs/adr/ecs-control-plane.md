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
3. Compute diff
4. Apply changes: create, update, or delete ECS resources
5. Write status back to S3 (separate prefix)
6. Sleep / wait for next event

---

## 3. Manifest Schema

```yaml
apiVersion: oab.dev/v1
kind: OABService
metadata:
  name: my-agent
  namespace: prod
spec:
  replicas: 2
  taskDefinition:
    image: 123456789.dkr.ecr.us-east-1.amazonaws.com/openab:latest
    cpu: 256
    memory: 512
    environment:
      - name: BACKEND_TYPE
        value: bedrock
  backend:
    type: bedrock
    model: anthropic.claude-sonnet-4-20250514
  networking:
    subnets: [subnet-abc, subnet-def]
    securityGroups: [sg-123]
    assignPublicIp: false
```

Key fields:
- `spec.taskDefinition` — maps directly to ECS RegisterTaskDefinition
- `spec.backend` — OAB-specific config injected as environment/secrets
- `spec.networking` — ECS awsvpc configuration

---

## 4. State Store Design (S3-Only)

```
s3://oab-control-plane/
  ├── manifests/{namespace}/{name}.yaml   ← desired state (oabctl writes)
  └── status/{namespace}/{name}.json      ← observed state (controller writes)
```

| Concern | Mechanism | Rationale |
|---------|-----------|-----------|
| Desired state | `s3://…/manifests/` | Human-readable, git-syncable, versioned via S3 versioning |
| Status / observed state | `s3://…/status/` | Controller writes after each reconcile cycle |
| Generation tracking | S3 object VersionId | Each `oabctl apply` creates a new version; controller compares |
| Change detection | S3 Event Notifications → EventBridge | Triggers controller on manifest PUT/DELETE |
| Consistency | S3 strong read-after-write | Sufficient for single-controller architecture |

**Why no DynamoDB in Phase 1:**
- Single controller instance — no leader election needed
- S3 strong read-after-write consistency (since Dec 2020) is sufficient
- Fewer moving parts, zero additional infra cost
- DDB can be added in Phase 2 for multi-replica leader election and fast status queries

---

## 5. CLI UX (`oabctl`)

### Core Commands

```bash
oabctl apply -f agent.yaml          # declare/update desired state
oabctl get oabservice               # list all services + status
oabctl get oabservice my-agent      # single service detail
oabctl delete oabservice my-agent   # mark for deletion
oabctl diff -f agent.yaml           # show local vs remote diff
oabctl logs my-agent                # shortcut to ECS task logs
oabctl wait my-agent --for=Available # block until condition met
```

### `apply` Semantics

```
$ oabctl apply -f prod/my-agent.yaml

✓ Schema validated
✓ Uploaded to s3://oab-control-plane/manifests/prod/my-agent.yaml
✓ Generation: v3 → v4
⏳ Waiting for reconciliation...
✓ Service my-agent reconciled (2/2 tasks running)
```

Behavior:
- Object doesn't exist → create (PUT to S3)
- Object exists → update (PUT overwrites, new S3 version)
- Immutable fields (namespace) → reject with error
- `--wait=false` to skip waiting for reconciliation

### `apply` Implementation (Phase 1)

```
oabctl apply -f agent.yaml
  │
  ├─ 1. Parse & validate YAML against schema (local, fast fail)
  ├─ 2. s3:PutObject → s3://oab-control-plane/manifests/{ns}/{name}.yaml
  └─ 3. (if --wait) Poll status/{ns}/{name}.json until phase=Running or timeout
```

No API server needed — `oabctl` talks directly to S3 via AWS SDK. Auth is standard IAM (role, profile, env vars).

---

## 6. Controller Lifecycle

### Reconcile Actions

| Diff | Action |
|------|--------|
| Manifest exists, no ECS service | RegisterTaskDefinition → CreateService |
| Manifest spec changed (new S3 version) | RegisterTaskDefinition (new revision) → UpdateService |
| Manifest deleted from S3 | UpdateService (desiredCount=0) → DeleteService → DeregisterTaskDefinition |
| ECS state drifted from spec | UpdateService to re-converge |

### Finalizers

Before deleting an ECS service, the controller:
1. Drains active connections (set desiredCount=0, wait for task stop)
2. Cleans up CloudMap service discovery entries
3. Removes the status JSON from S3
4. Only then acknowledges deletion

---

## 7. MVP Scope

### Phase 1 (minimal viable control plane)

1. **S3 bucket** — manifests/ and status/ prefixes, versioning enabled
2. **Single-instance ECS task** running the controller
3. **Poll-based** — ListObjects + GetObject every 30s
4. **`oabctl apply`** — validates and uploads YAML to S3
5. **`oabctl get`** — reads status/ prefix, displays table
6. Support `create` and `update` only (delete = manual remove from S3)

### Phase 2

- Event-driven triggers (S3 → EventBridge → controller wakes immediately)
- `oabctl delete` with finalizer support
- `oabctl diff` and `oabctl logs`
- DynamoDB for leader election (multi-replica controller)
- Rollback via S3 version history (`oabctl rollback my-agent --to-version=v3`)

### Phase 3

- Multi-region (controller per region, shared manifest bucket with replication)
- Dependency graph (service A depends on service B)
- Auto-scaling policies in manifest spec
- GitOps integration (GitHub Actions → `oabctl apply` on push)

---

## 8. Alternatives Considered

| Alternative | Why not chosen |
|-------------|---------------|
| AWS Proton | Opinionated, limited customization for OAB-specific logic |
| AWS Copilot | Good for simple apps, no custom reconciliation loop |
| CDK Pipelines | Deployment tool, not a runtime controller with drift detection |
| Step Functions orchestrator | Stateless execution model, no continuous reconciliation |
| Run K8s anyway (EKS) | Valid but adds operational overhead for teams that chose ECS |
| DynamoDB as primary store | Adds infra; S3 sufficient for single-controller Phase 1 |

---

## 9. Open Questions

1. **Secrets management** — inject via ECS Secrets (SSM/SecretsManager reference in task def) or controller-managed?
2. **Multi-region** — single controller per region, or global controller with regional reconcilers?
3. **Observability** — CloudWatch metrics from the controller, or push to a shared OAB dashboard?
4. **Upgrade strategy** — how does the controller upgrade itself without downtime?
5. **Networking isolation** — shared VPC or per-service security group rules?

---

## 10. Decision

We adopt the CRD + Operator pattern on ECS with an **S3-only state store** and a **`oabctl` CLI** for the operator interface. The controller runs as a single ECS service that reconciles OABService manifests (stored in S3) against actual ECS state. DynamoDB is deferred to Phase 2 when multi-replica HA is needed. This gives ECS-native teams the same declarative, self-healing deployment experience that K8s operators provide — with minimal infrastructure footprint.

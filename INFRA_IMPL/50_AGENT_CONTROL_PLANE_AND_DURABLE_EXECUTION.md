# Step 50 - Agent Control Plane and Durable Execution

## Goal

Define how this platform can evolve from a strong multi-tenant Wasm execution
substrate into a usable platform for agentic workloads.

This document is not about "can an agent run here at all?" The answer to that
is already yes for HTTP and event-driven apps.

The real question is:

```text
What additional system components are required so the platform can host
reliable, multi-tenant, observable, policy-controlled agents at production
quality?
```

The answer is:

- the current platform is a good execution data plane,
- it is not yet a full agent platform,
- the missing piece is an agent control plane with durable async execution.

---

## Executive Summary

### What the current platform is already good at

The existing architecture is already well aligned with several agent use cases:

- multi-tenant app isolation,
- HTTP ingress through the proxy,
- dynamic instance startup and scale-out,
- per-app policy controls,
- secret handling,
- event-based coordination through NATS,
- artifact-based deployment and versioning,
- host/path routing for public APIs.

This is a strong base for:

- API-facing agents,
- webhook-driven agents,
- tool gateway services,
- stateless orchestration services,
- event-triggered workers,
- tenant-isolated agent backends,
- lightweight multi-agent services that externalize memory and workflow state.

### What is missing

The platform does not yet provide first-class support for:

- durable background tasks,
- retries and delayed work,
- cron/scheduled execution,
- task leasing/ownership,
- resumable multi-step workflows,
- waiting for tool results or human approval,
- run-level state and checkpoints,
- per-agent concurrency and budget control,
- session-aware observability.

Those are not "nice to have" features for production agent systems. They are
the minimum control-plane behaviors that keep agent execution reliable.

### Main design decision

Do not turn the base runtime/proxy/supervisor into agent-specific logic.

Instead:

```text
Current platform = agent execution data plane
New components   = agent control plane
```

That keeps the system general-purpose while enabling agent use cases on top.

---

## Current Platform Assessment

### Existing components that already help

The current repository already contains the building blocks for an agent data
plane:

- `proxy`:
  - ingress routing,
  - host/path dispatch,
  - auth/rate-limit/circuit-breaker support,
  - multi-app front-door behavior.

- `node` + `supervisor` + `runtime`:
  - actual app execution,
  - isolation,
  - instance lifecycle,
  - cold-start and scale behavior.

- `messaging` + NATS/JetStream:
  - event transport,
  - cluster fanout,
  - decent backbone for async work dispatch.

- `storage`:
  - durable local state for platform records.

- `secrets`:
  - tenant/app credential isolation.

- `billing`, `metrics`, `logging`, `policy`:
  - the right operational primitives for tracking and limiting agent workloads.

### Where the current platform stops

The current platform can deploy an "agent app", but it does not natively manage
agent runs.

It can answer:

- where should this app run?
- how do requests reach the app?
- how do we isolate and scale it?

It cannot yet answer:

- what work item should run next?
- which worker owns this run right now?
- what happens if the worker dies mid-tool-call?
- how do we retry or back off?
- how do we resume after a wait state?
- how do we budget an agent run across multiple steps?

That gap is exactly what the control plane must close.

---

## What "Agent Platform" Means Here

For this platform, "agent platform" should mean:

1. Operators can deploy and version agent applications.
2. Tenants can create agent definitions and agent runs.
3. Runs can be synchronous or asynchronous.
4. Asynchronous runs are durable and resumable.
5. Agents can call tools under policy.
6. Human approval or external callbacks can pause and resume runs.
7. Every run is observable, auditable, and quota-controlled.
8. Agent memory is externalized rather than trusted to a single process.

That definition is intentionally narrower than "general AI platform" and much
more useful for implementation.

---

## Workload Classes

Not all agent workloads need the same system shape.

### Class A - Synchronous request/response agents

Examples:

- "answer this question",
- "summarize this payload",
- "call these tools and return one response",
- "chat completion behind an API route".

Characteristics:

- bounded request lifetime,
- external memory optional,
- simpler retry model,
- usually HTTP only.

The current platform already supports this class reasonably well.

### Class B - Asynchronous task agents

Examples:

- document processing,
- research jobs,
- integration tasks,
- report generation,
- autonomous background workers.

Characteristics:

- work lasts longer than one HTTP request,
- retries are normal,
- task ownership matters,
- partial progress must survive process/node failure.

This class requires queueing and durable run tracking.

### Class C - Durable multi-step workflow agents

Examples:

- plan -> tool call -> wait -> evaluate -> tool call -> finalize,
- human-in-the-loop approval flows,
- multi-agent delegation,
- long-running investigations or business operations.

Characteristics:

- explicit step graph/state machine,
- waiting states,
- resumability,
- checkpointing,
- rich audit trail.

This class requires a true orchestration layer, not just a queue.

### Class D - Scheduled and recurrent agents

Examples:

- daily research jobs,
- hourly syncs,
- periodic monitoring agents,
- recurring maintenance tasks.

Characteristics:

- cron or interval trigger,
- deduplication,
- no overlapping execution guarantees in some cases.

This class requires a scheduler on top of the durable execution model.

---

## Control Plane Responsibilities

The agent control plane should own the parts of the system that decide,
coordinate, and track agent execution.

### 1. Agent registry

Store and manage:

- agent identity,
- owning tenant,
- current version,
- deployed app target,
- model/tool config references,
- default runtime limits,
- ingress exposure mode,
- async/sync capability,
- approval and policy requirements.

### 2. Run intake

Accept new run requests from:

- HTTP API,
- internal service calls,
- queue events,
- schedules,
- webhooks.

Responsibilities:

- authenticate the caller,
- validate tenant ownership,
- allocate a run ID,
- persist initial run state,
- route to sync or async execution path.

### 3. Scheduling and dispatch

For async work, the control plane must:

- enqueue runnable work,
- apply priority/fairness,
- enforce per-tenant concurrency,
- enforce per-agent concurrency,
- hand work to workers through a lease/claim model,
- requeue timed-out work,
- support delay/backoff/retry windows.

### 4. Durable run state

Persist run state transitions such as:

- `queued`,
- `claimed`,
- `running`,
- `waiting_for_tool_result`,
- `waiting_for_human`,
- `waiting_for_timer`,
- `completed`,
- `failed`,
- `cancelled`.

This must survive:

- worker crash,
- node restart,
- NATS reconnect,
- rolling upgrades.

### 5. Workflow orchestration

The control plane must support:

- step transitions,
- branching,
- fanout,
- joins,
- pauses,
- resumptions,
- cancellation,
- timeout handling.

For the first implementation, this can be a state machine rather than a
full DAG engine.

### 6. Tool policy and execution contracts

The control plane should govern:

- which tools the agent may call,
- network egress rules,
- credential binding,
- timeouts,
- tool result size limits,
- tool call auditing,
- whether a tool is sync, async, or requires approval.

### 7. Session and memory coordination

The control plane should not become the memory store itself, but it should
coordinate references to:

- conversation/session IDs,
- external vector stores,
- relational state,
- intermediate artifacts,
- tool transcripts.

### 8. Observability and accounting

Track:

- per-run event timeline,
- step durations,
- retries,
- tool invocation logs,
- token/cost/accounting estimates,
- resource and concurrency usage by tenant/agent.

---

## Why a Scheduler Matters

The user question behind this document is important:

```text
Does an agent platform need a scheduler?
```

### Short answer

Not for the simplest synchronous agents.

Yes for durable asynchronous agent systems.

### Why

Once work can outlive one request, the system must decide:

- when a task is eligible,
- who owns it,
- when it should retry,
- what happens if the owner disappears,
- how to avoid duplicate execution,
- how to respect quotas and fairness.

That is scheduler behavior, even if the implementation starts as a queue with
lease-based claiming.

### Minimal scheduler requirement

The first version does not need a complex Kubernetes-style scheduler.

It needs a practical control-plane scheduler with:

- queue,
- lease,
- visibility timeout,
- retry count,
- backoff,
- priority,
- delayed execution timestamp.

This is enough to support many agent systems.

---

## Recommended Architecture

## Layer split

```text
North-South ingress
  -> Proxy
  -> Agent API / Agent Worker Apps

Agent control plane
  -> Agent Registry
  -> Run API
  -> Scheduler / Queue
  -> Workflow Orchestrator
  -> Policy Engine
  -> Run Store / Event Store

Execution data plane
  -> Existing node/supervisor/runtime system

External state
  -> Postgres / redb / object store / vector store / NATS streams
```

## New logical components

### 1. `agent-registry`

Purpose:

- define agents independent of raw deployed app IDs,
- resolve `agent_id -> deployed app version`,
- hold policy defaults and metadata.

### 2. `run-orchestrator`

Purpose:

- own the lifecycle of an agent run,
- create run records,
- enqueue first step,
- evaluate step results,
- decide next transition,
- complete/fail/cancel runs.

### 3. `job-scheduler`

Purpose:

- queue runnable work,
- manage lease-based claims,
- enforce concurrency,
- support retries and delayed work,
- drive scheduled tasks.

### 4. `tool-policy`

Purpose:

- map tools to policies,
- attach credentials safely,
- validate which agent/tenant may call which tool,
- centralize audit and budget rules.

### 5. `run-store`

Purpose:

- persist run metadata,
- state transitions,
- checkpoints,
- step events,
- tool-call records,
- operator-visible history.

### 6. `schedule-service`

Purpose:

- create recurring triggers,
- translate cron/interval rules into runnable jobs,
- apply dedupe and overlap policy.

This can be a later phase, but the model should be anticipated from the start.

---

## Mapping to the Current Repo

The cleanest approach is to add new crates rather than force agent semantics
into `proxy`, `runtime`, or `supervisor`.

Recommended additions:

```text
crates/agent-registry
crates/agent-control-plane
crates/agent-scheduler
crates/agent-run-store
crates/agent-tools
```

### Existing crates that would be reused heavily

- `common`
  - shared types,
  - auth/billing/policy records,
  - agent run/event schemas.

- `messaging`
  - NATS subjects for run events and work claims.

- `proxy`
  - public API exposure for synchronous agent endpoints.

- `node` / `supervisor` / `runtime`
  - run the actual agent worker apps.

- `storage`
  - local control-plane persistence for prototype/single-node mode.

- `billing`
  - run accounting and tenant usage tracking.

### What should not be overloaded

Do not put workflow orchestration inside:

- the proxy,
- the node handler event switch,
- the supervisor instance manager,
- the Wasm runtime.

Those layers are the wrong abstraction boundary.

---

## Data Model

The platform needs agent-specific records beyond `AppConfig`, `Route`, and
deploy metadata.

## 1. Agent definition

```rust
pub struct AgentDefinition {
    pub agent_id: String,
    pub tenant_id: String,
    pub app_id: AppId,
    pub version: String,
    pub mode: AgentMode, // SyncApi | AsyncWorker | Hybrid
    pub default_model_ref: Option<String>,
    pub tool_policy_ref: Option<String>,
    pub memory_backend_ref: Option<String>,
    pub max_concurrent_runs: u32,
    pub max_run_duration_secs: u64,
    pub enabled: bool,
    pub created_at: u64,
    pub updated_at: u64,
}
```

## 2. Agent run

```rust
pub struct AgentRun {
    pub run_id: String,
    pub tenant_id: String,
    pub agent_id: String,
    pub app_id: AppId,
    pub status: AgentRunStatus,
    pub trigger: RunTrigger,
    pub session_id: Option<String>,
    pub input_ref: Option<String>,
    pub output_ref: Option<String>,
    pub checkpoint_ref: Option<String>,
    pub retry_count: u32,
    pub priority: u8,
    pub created_at: u64,
    pub updated_at: u64,
    pub started_at: Option<u64>,
    pub completed_at: Option<u64>,
}
```

## 3. Work item

```rust
pub struct WorkItem {
    pub work_id: String,
    pub run_id: String,
    pub step_id: String,
    pub queue: String,
    pub status: WorkItemStatus,
    pub eligible_at: u64,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<u64>,
    pub attempts: u32,
    pub max_attempts: u32,
    pub backoff_policy: BackoffPolicy,
    pub created_at: u64,
    pub updated_at: u64,
}
```

## 4. Run event

```rust
pub struct AgentRunEvent {
    pub run_id: String,
    pub seq: u64,
    pub event_type: AgentRunEventType,
    pub payload_json: String,
    pub timestamp: u64,
}
```

## 5. Tool call record

```rust
pub struct ToolCallRecord {
    pub tool_call_id: String,
    pub run_id: String,
    pub tool_name: String,
    pub status: ToolCallStatus,
    pub request_ref: Option<String>,
    pub response_ref: Option<String>,
    pub started_at: u64,
    pub completed_at: Option<u64>,
}
```

---

## State Machine

The first implementation should use a clear state machine rather than implicit
"best effort" transitions.

## Run-level states

```text
Created
Queued
Claimed
Running
WaitingForTool
WaitingForHuman
WaitingForTimer
Completed
Failed
Cancelled
```

## Step-level pattern

```text
Run created
  -> scheduler creates runnable work item
  -> worker claims work item
  -> agent executes step
  -> emits one of:
       - step completed
       - tool requested
       - human approval requested
       - retryable failure
       - terminal failure
  -> orchestrator persists transition
  -> orchestrator enqueues next step if needed
```

This design makes crashes recoverable because the system's truth is in durable
state, not inside one process memory space.

---

## Messaging Model

NATS/JetStream is a reasonable fit for the first implementation, but it should
not be the sole source of truth for workflow state.

Recommended principle:

```text
JetStream = transport + dispatch
Run store  = durable truth
```

### Suggested subjects

```text
agents.run.create
agents.run.enqueue
agents.work.claim
agents.work.start
agents.work.heartbeat
agents.work.complete
agents.work.fail
agents.run.transition
agents.tool.request
agents.tool.result
agents.run.cancel
agents.schedule.tick
```

### Important rule

Never assume that "message acknowledged" means "run safely persisted".

Persist the run/work state first or within a controlled transactional boundary,
then publish follow-up events.

---

## Storage Strategy

### Phase 1

Use the existing storage model plus a dedicated agent run store table set.

Good for:

- local development,
- single-node control plane,
- fast iteration.

### Phase 2

Move agent run state to a stronger shared store for production durability and
queryability.

Likely options:

- Postgres for run state and indexing,
- object store for large artifacts/transcripts,
- vector store only when needed by the workload,
- JetStream retained events for dispatch/replay support.

### Why not keep everything in process memory

Agent systems fail in ways that normal request/response apps do not:

- tool calls time out,
- humans do not answer immediately,
- jobs last minutes or hours,
- workers restart mid-execution,
- retries and dedupe become normal behavior.

Without durable state, the system will lose control of runs.

---

## Tool Execution Model

Tool invocation needs to be treated as a first-class controlled action.

## Tool classes

### Inline tools

Examples:

- formatting,
- deterministic transforms,
- local small business rules.

These can run inside the worker app process.

### Remote tools

Examples:

- external APIs,
- search providers,
- SaaS integrations,
- browsers,
- code execution sandboxes,
- model gateways.

These need:

- egress policy,
- credential binding,
- timeout limits,
- output size limits,
- request/response audit.

### Deferred tools

Examples:

- human approval,
- async batch search,
- long-running external job.

These should transition the run into a waiting state and resume later.

## Tool policy record

```rust
pub struct ToolPolicy {
    pub tenant_id: String,
    pub agent_id: String,
    pub allowed_tools: Vec<String>,
    pub allowed_domains: Vec<String>,
    pub max_tool_timeout_secs: u64,
    pub require_human_approval_for: Vec<String>,
    pub credential_bindings: Vec<ToolCredentialBinding>,
}
```

---

## Memory and State Model

The platform should assume that agent memory is external.

The worker app should not be the durable holder of:

- conversation history,
- research corpus,
- vector embeddings,
- long-lived scratchpad,
- task backlog.

### Recommended split

- control plane:
  - stores run metadata and orchestration state.

- external memory systems:
  - store semantic memory and business data.

- worker:
  - loads only the state needed for the current step.

This makes scaling and recovery significantly easier.

---

## Scheduling and Fairness

A multi-tenant agent platform must not dispatch work greedily.

At minimum, the scheduler needs:

- per-tenant max concurrent runs,
- per-agent max concurrent runs,
- queue priority,
- retry backoff,
- lease expiry,
- starvation prevention,
- cancellation support.

### Recommended initial scheduling policy

Use a simple weighted fair scheduler:

- tenant-level concurrency bucket,
- then per-agent bucket,
- then priority order inside each bucket,
- then FIFO within equal priority.

This is straightforward to implement and good enough initially.

---

## Failure Model

This platform should assume failures are routine.

## Expected failures

- worker crashes mid-run,
- node restarts,
- NATS disconnects,
- duplicate event delivery,
- external tool timeout,
- external tool partial success,
- operator cancellation,
- deploy while runs are active.

## Required guarantees

### At minimum

- work claims must expire,
- retries must be bounded,
- terminal failures must be visible,
- duplicate completion events must be idempotent,
- run transitions must be monotonic and validated.

### Strong recommendation

Every externally visible action should be idempotent or deduplicated by key:

- run creation,
- work claim,
- tool request,
- callback processing,
- human approval submission.

---

## API Shape

The control plane should expose APIs above raw deployed apps.

## Suggested endpoints

```text
POST   /agents
GET    /agents/{agent_id}
POST   /agents/{agent_id}/runs
GET    /runs/{run_id}
POST   /runs/{run_id}/cancel
POST   /runs/{run_id}/resume
GET    /runs/{run_id}/events
POST   /tools/{tool_call_id}/callback
POST   /schedules
GET    /tenants/{tenant_id}/agents
```

## Example flow

```text
POST /agents/support-bot/runs
  -> create run
  -> enqueue work
  -> worker claims work
  -> worker emits tool request
  -> orchestrator stores waiting state
  -> callback arrives
  -> orchestrator resumes run
  -> worker completes
  -> run marked completed
```

---

## Security and Policy

Agentic systems expand the blast radius of "application code" because they can
actively decide to call tools or perform follow-up actions.

That means the platform must add stronger controls than a normal app-only PaaS.

## Required controls

- per-tenant tool allowlists,
- outbound domain restrictions,
- credential scoping by tenant/agent/tool,
- run time budget limits,
- token/cost ceilings,
- approval gates for dangerous tools,
- immutable audit events,
- operator-visible cancellation and quarantine,
- stricter secret separation between deploy-time, runtime, and tool-time use.

## Strong recommendation

Treat tool credentials as a separate class from ordinary app runtime secrets.

Why:

- tools often map to sensitive side effects,
- different agents may share one deployed app version,
- credential least privilege is easier to enforce at control-plane binding time.

---

## Observability Model

Normal HTTP metrics are not enough.

The platform needs run-centric visibility.

## Core observability objects

- run timeline,
- step timeline,
- tool call timeline,
- scheduler wait time,
- retry count,
- queue depth,
- active leases,
- completion/failure/cancel counts,
- per-tenant concurrency saturation.

## Example metrics

```text
agent_runs_created_total
agent_runs_completed_total
agent_runs_failed_total
agent_runs_cancelled_total
agent_run_duration_seconds
agent_queue_depth
agent_work_claim_latency_seconds
agent_tool_calls_total
agent_tool_call_duration_seconds
agent_waiting_runs_total
agent_scheduler_retries_total
```

## Logging requirements

Every run should be traceable through:

- `tenant_id`,
- `agent_id`,
- `run_id`,
- `step_id`,
- `tool_call_id`,
- `app_id`,
- `node_id`,
- `instance_id`.

---

## Billing and Quotas

The existing billing direction in the repo is useful here, but agent workloads
need a richer accounting model.

## What should be billable

- run count,
- execution duration,
- tool calls,
- model token usage when available,
- external service usage,
- storage footprint for retained run data.

## Quotas to enforce

- max concurrent runs per tenant,
- max queued runs per tenant,
- max run duration,
- max retries per run,
- max tool calls per run,
- max aggregate monthly run budget.

---

## Deployment Model for Agent Apps

The current deploy system should remain the mechanism for shipping agent worker
applications.

What changes is the logical contract above it.

## Recommendation

Continue to deploy agent implementations as normal Wasm apps with:

- public routes if they expose sync APIs,
- internal-only mode if they are worker-only,
- gateway policy if needed,
- namespace isolation,
- tool/network policy references in config.

The control plane should reference deployed `app_id` values rather than
inventing a separate execution substrate.

This keeps the implementation aligned with the current platform strengths.

---

## Suggested Phased Implementation

## Phase 1 - Agent hosting baseline

Deliver:

- `AgentDefinition`,
- `AgentRun`,
- simple run creation API,
- sync request/response mode,
- async queue with one-step work items,
- lease-based claims,
- per-tenant concurrency limits,
- run/event persistence,
- basic run inspection API.

Use case unlocked:

- tenant-isolated background jobs,
- API-facing agents,
- webhook-triggered workers.

## Phase 2 - Durable step orchestration

Deliver:

- waiting states,
- resumable runs,
- tool request/result model,
- cancellation,
- retries with backoff,
- richer run timelines,
- operator controls.

Use case unlocked:

- real multi-step agents,
- async external tool integrations,
- partial failure recovery.

## Phase 3 - Scheduling and recurrence

Deliver:

- cron/interval schedules,
- delayed jobs,
- overlap policies,
- recurring run generation,
- fairness improvements.

Use case unlocked:

- autonomous scheduled agents,
- periodic maintenance and monitoring jobs.

## Phase 4 - Advanced multi-agent support

Deliver:

- delegation/sub-run model,
- parent/child run relationships,
- fanout/join patterns,
- shared session graph,
- approval workflows.

Use case unlocked:

- orchestrator-worker agent systems,
- collaborative specialized agents.

---

## Acceptance Criteria

The platform can be considered agent-ready for production only when all of the
following are true:

- an agent can be deployed as a normal Wasm app,
- a tenant can create an agent definition independent of raw app deployment,
- a run can be created and tracked durably,
- a worker crash does not lose run ownership permanently,
- retries and backoff are enforced by the platform,
- a run can pause and resume after external input,
- tool usage is policy-controlled and auditable,
- per-tenant concurrency and budget limits are enforced,
- operators can inspect, cancel, and diagnose runs,
- scheduling exists for delayed and recurring work.

---

## Recommended First Implementation in This Repo

If the goal is to start implementing now, the best first vertical slice is:

1. Add `AgentDefinition`, `AgentRun`, and `WorkItem` types in `common`.
2. Add persistent tables in `storage`.
3. Add `agent-control-plane` crate exposing:
   - `POST /agents/{agent_id}/runs`
   - `GET /runs/{run_id}`
   - `POST /runs/{run_id}/cancel`
4. Add `agent-scheduler` crate:
   - enqueue,
   - claim with lease,
   - complete/fail,
   - retry with backoff.
5. Reuse NATS for dispatch messages.
6. Run agent workers as ordinary deployed apps using current deploy/proxy paths.
7. Add run-centric metrics and logs.

This delivers useful agent hosting quickly without prematurely building a
massive workflow system.

---

## Final Recommendation

This platform should absolutely target agent use cases, but it should do so by
adding a durable control plane above the existing execution layer.

Do not try to solve agent orchestration by:

- adding ad hoc retry loops to apps,
- depending only on raw HTTP requests,
- treating NATS delivery as durable run state,
- assuming the worker process is the source of truth,
- pushing scheduler logic into the proxy or supervisor.

The right model is:

```text
existing platform = execution substrate
new control plane = durable agent coordination
```

That architecture is compatible with:

- simple synchronous agents,
- async workers,
- scheduled jobs,
- multi-step tools,
- human approval gates,
- multi-agent orchestration,
- strong tenant isolation,
- future billing and policy controls.

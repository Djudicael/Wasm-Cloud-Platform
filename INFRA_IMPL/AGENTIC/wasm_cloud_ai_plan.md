<!-- Current ownership: see the correction in section 5; M19 platform allocations below are superseded planning context. -->
# Wasm Cloud AI Platform — Architecture and Implementation Plan

**Date:** 2026-10-30  
**Status:** Proposed umbrella plan; nothing described here as planned is implemented  
**Companions:** [INVESTIGATION.md](INVESTIGATION.md), [PLATFORM_HOSTED_HARNESS.md](PLATFORM_HOSTED_HARNESS.md), [PLATFORM_MODIFICATIONS.md](PLATFORM_MODIFICATIONS.md), [TOOL_CATALOG.md](TOOL_CATALOG.md)  
**Track A detail:** the harness repository's `docs/coding-agent-plan.md`  
**Goal:** Reduce total cost per **verified successful task** through code intelligence, a validation harness, WASI tools, and, as a parallel research track, Rust-native inference.

> Revision note: the first draft described a single greenfield workspace with
> its own gateway, orchestrator, event schema, and WIT inference imports. It was
> revised to build on the existing harness repository, respect the recorded
> platform/harness boundary, move inference behind an HTTP contract, and make
> the native job runner an external harness-owned service. The coding-agent
> detail (verification, ingestion, code intelligence, integrity, governance,
> operations, evaluation, walking skeleton, user experience, tasks and sizes)
> now lives in the harness repository, which owns that work. This document
> keeps the cross-track structure, the platform prerequisites, and the
> inference track until that track has its own repository.

## Contents

| Part | Sections |
| --- | --- |
| Scope | [0 Ownership](#0-ownership-and-tracks), [1 Goal metric](#1-goal-metric), [2 Architecture](#2-architecture), [3 Layout](#3-repository-layout) |
| Contracts | [4 Inference boundary](#4-inference-boundary), [5 Native job runner](#5-native-job-runner-platform-m19) |
| Track A | [6 Coding agent summary](#6-track-a-summary) |
| Track C | [7 Engine](#7-native-inference-engine-track-c), [8 Compiler](#8-mlir--openxla--stablehlo--pjrt-track-c), [9 KV cache](#9-distributed-kv-cache-and-cost-aware-routing-track-c) |
| Cross-track | [10 Deployment](#10-deployment-topology), [11 Security](#11-cross-track-security), [12 Roadmap](#12-roadmap-and-delivery-gates), [13 Benchmarks](#13-cross-track-benchmarks), [14 Risks](#14-risks-and-mitigations), [15 ADRs](#15-adr-backlog), [16 Tasks](#16-platform-and-inference-tasks), [17 Done](#17-definition-of-done), [18 Open questions](#18-open-questions) |

Numeric values are starting points to calibrate against measurements, not commitments.

## 0. Ownership and tracks

Three projects, three independent tracks. Only Track A is on the critical path to a useful coding agent.

| Track | Repository | Owns | Must not own |
| --- | --- | --- | --- |
| **A — Coding agent** | Harness repository | Agent API, coordinator, harnesses, broker and tools, model gateway, workspace service, code intelligence, validation, evaluation, migrations, agent rehearsal scripts | Runtime, network, or identity features; inference engines |
| **B — Platform** | This repository | General-purpose execution capabilities: runtime limits, triggers, egress policy, gateways, isolation, generic service microVM patterns, observability | Any agent concept, agent WIT interfaces, `wasi-nn`, inference code |
| **C — Inference R&D** | New repository (name to be decided); described here until it exists | Tensor backends, model execution, serving scheduler, KV cache, compiler backends, inference routing, inference benchmarks | Agent logic; platform runtime changes |

The tracks meet only at published contracts:

- Track A runs on Track B as ordinary `wasm32-wasip2` applications.
- Track A reaches Track C over HTTP through the harness model gateway (section 4), using an explicit external endpoint, never `.internal`.
- Native checks run in the external harness-owned job service (section 5; supersedes M19), reached the same way.

## 1. Goal metric

```text
cost per verified success = total cost of ALL attempted tasks in a suite
                            / number of verified successes
```

A verified success passes independent verification on a clean base: the patch applies, builds and passes visible tests, passes hidden tests (evaluation) or human acceptance (production), and passes test-integrity rules. Failed and abandoned tasks count toward cost. The verification levels, cost components and price tables are defined in the harness plan; every track reports against this metric:

| Track | How it moves the metric |
| --- | --- |
| A | Higher success and fewer tokens per task through retrieval, validation, repair, caching and cascades |
| B | Enables external-service access, lower overhead (deadlines, lifecycle) and cost evidence (traces, usage reports) |
| C | Lower cost per token, at matched task success, when volume justifies self-hosting (section 12) |

## 2. Architecture

```text
User / IDE / API / optional MCP clients
    -> Platform edge proxy + OIDC Hub (auth, rate limits)         [platform, exists]
    -> Agent API / operator API                                   [harness, exists]
    -> Coordinator (runs, leases, checkpoints, intents, approvals) [harness, exists]
        |-> Versioned harness step (stateless proposal)           [harness, exists]
        |-> Worker + tool broker                                  [harness, exists]
        |     |-> pure WASI tools (text, validation, parsing)
        |     |-> workspace service (revisioned snapshots)        [harness, planned]
        |     |-> code intelligence (Tree-sitter, index, context) [harness, planned]
        |     |-> check-runner adapter -------------------------> Native job service [harness-owned, local implementation]
        |-> Model gateway (internal contract, policy, cascade)    [harness, partly exists]
              |-> OpenAI-compatible adapter -> OpenRouter / local servers   [exists]
              |-> native adapter ---------------------------------> Rust inference service [Track C]
                                                                      |-> tokenizer / model loader
                                                                      |-> prefill / decode scheduler
                                                                      |-> KV cache / sampling
                                                                      |-> backends: custom CPU/SIMD,
                                                                          Candle, custom CUDA,
                                                                          MLIR -> StableHLO -> OpenXLA/PJRT
    -> PostgreSQL (state, indexes) + RustFS / S3 (artifacts)      [service microVMs]
    -> Observability stack (traces, logs, metrics, alerts)        [platform]
    -> Evaluation runner / task miner / optional dataset builder  [harness, planned]
```

**Principles**

- Models propose; the coordinator validates and the broker authorizes. Model output grants no authority.
- A harness is a stateless step function: it receives a checkpoint and returns a typed proposal. It has no database credentials, provider keys, or unrestricted tool access.
- The inference engine stays native Rust in its own service. WASI guests reach it over HTTP. No custom WIT inference imports and no `wasi-nn` in the platform runtime.
- Agents are ordinary platform applications. Platform features stay general-purpose.
- `.internal` mesh traffic is node-local (`every_node` placement plus same-namespace `local_dependencies`). Off-node services (inference, job runner, PostgreSQL, object storage) use explicit external endpoints under the app's network profile.
- Verification is independent of the agent: a fresh job on a clean base decides success.
- Crates are logical boundaries, **not automatically separate network services**.

**Non-goals for the MVP:** training a foundation model; implementing every vLLM feature; GPU kernels inside portable WASI; mandatory remote KV transfer; assuming StableHLO includes PagedAttention; sending full serialized ASTs to models; a second API gateway or orchestrator; Kubernetes for agent workloads; a pod or VM per agent session; languages other than Rust; pushing to default branches.

## 3. Repository layout

**Track A** — the existing harness crates (`agentic-core`, `agentic-repository`, `agentic-coordinator-api`, `agentic-service`, `agentic-worker-api`, `agentic-scheduler-api`, document, auth and development crates) plus planned workspace, ingestion, code-index, context, model-gateway, check-runner, integrity, evaluation and optional MCP crates. See the harness plan.

**Track B** — no new crates are required by this plan. The native job API, image and lifecycle belong to Track A in the harness repository; no platform job service is added.

**Track C** — new inference repository:

```text
crates/
  inference-api/            OpenAI-compatible and native HTTP endpoints
  inference-core/           InferenceEngine trait, capabilities, lifecycle
  inference-models/         model registry, Safetensors, tokenizer
  inference-model-llama/    first dense decoder-only model
  inference-scheduler/      prefill/decode scheduling, batching, cancellation
  inference-router/         model- and prefix-aware worker routing
  tensor-core/ tensor-cpu/ tensor-candle/ tensor-cuda/
  graph-ir/ graph-mlir-bridge/ graph-openxla-pjrt/
  kv-runtime/ kv-transport/ kv-distributed/
benchmarks/ tests/ docs/adr/
```

Use architecture decision records and versioned interfaces in all three repositories.

## 4. Inference boundary

The boundary between agents and inference is an **HTTP service**, not a WIT host import:

| Option | Assessment |
| --- | --- |
| WIT host import (`cloud:inference`) | Rejected. Couples the Wasmtime runtime to inference, places GPU and C++ code in platform nodes, is synchronous, has no remote transport, and contradicts the "no agent concept in the runtime" decision |
| `wasi-nn` | Rejected. In-host execution without batching or scheduling control |
| **HTTP service** | Chosen. Isolates GPU and compiler failures from platform nodes, scales on its own hardware, and uses the existing policy-controlled `wasi:http` egress with no platform change |

OpenAI-compatible Chat Completions is a **compatibility layer, not the internal contract**. It is a convention rather than a specification, behaviour varies by provider, and it does not express idempotency, explicit cache identity, token IDs, standard constrained decoding, or explicit cancellation.

- **Harness side:** the model gateway owns the internal contract (operation ID, tenant, data class, profile, budget, cache key, usage and cost) and its adapters. Defined in the harness plan.
- **Track C side:** the inference service exposes
  - `/v1/chat/completions` (OpenAI-compatible), so the same harness can benchmark it against vLLM or llama.cpp;
  - an extended native endpoint adding an idempotency key, cache key, explicit cancellation, grammar or schema-constrained decoding, and token-level usage.

Start with the OpenAI-compatible endpoint only. Add each extended field when a benchmark shows it matters; for example, add `cache_key` with prefix-aware routing (section 9), not before.

**Constraints today:** no exactly-once inference (a retried request may be charged twice); streaming through platform gateways requires M9; token usage enters platform billing only after M14.

**Acceptance:** The same harness and agent definition run unchanged against a provider, a local compatible server and the Track C service, selected only by model profile configuration.

## 5. Native job runner (platform M19)

**Ownership correction (2026-10-30).** The historical platform allocation below
is superseded: the external native job service belongs to
wasmcloud_platform_harness, including API, images, microVM lifecycle and adapter.
The platform only executes WASI and supplies ordinary outbound HTTP policy.
See [the current harness guide](../../../wasmcloud_platform_harness/docs/microvm-checks.md).


The coding agent is verified only by compiling and testing patches, which needs native process execution. Neither the WASI runtime nor the current single trust domain provides it. This is an external harness service; the earlier platform allocation was specified as [M19](PLATFORM_MODIFICATIONS.md#m19-native-job-runner-service-microvm-p2).

| Concern | Platform (this repository) | Harness |
| --- | --- | --- |
| Service | Generic WASI execution and outbound policy | Native Linux service, image, per-job microVM lifecycle and validation |
| Job API | No agent job API | Submit, status, bounded results, dedicated OIDC identity and `checks-run-v1` adapter |
| Commands | No native commands | Fixed service profiles; planned broader set: `cargo fmt --check`, `cargo check`, `cargo clippy`, `cargo test`, rust-analyzer diagnostics, repository clone |
| Isolation | Platform WASI isolation remains unchanged | Fresh job microVM; never forwards coordinator, NATS, database or storage credentials |
| Network | Explicit external endpoint under app policy | No job NIC; vendored sources first, registry and Git access remain planned |
| Caches | No job cache ownership | Fresh job disks first; tenant-scoped caches remain planned |

The implemented first form is an external native service launching a fresh networkless microVM per job, keeping untrusted builds off platform nodes. Local microVM validation is not a production isolation guarantee.

## 6. Track A summary

The harness plan defines Track A in full. Its phases and their platform needs:

| Phase | Deliverable | Platform needs |
| --- | --- | --- |
| A-skel — Walking skeleton | Archive import, grep/read, patch validation and publication on the existing loop; five tasks | M15 for local rehearsal |
| A0 — Baseline | Cost ledger, trajectory events and replayable payloads, mined task suite, baseline agent | M19 for full verification; Vault Transit service for optional payload encryption rehearsal |
| A1 — Workspace | Revisions, branches, ingestion, data classes, tenant model | M15, M8 |
| A2 — Code intelligence | Index, `code.*`, `context.build` | M8 |
| A2+ — Optional MCP | Read-only MCP endpoint for external agents | M9 for streaming only |
| A3 — Checks and repair | Check-runner adapter, integrity rules, stop conditions, cascade | M19, M10 |
| A4 — Evaluation and delivery | Evaluation report, PR delivery, CLI client | M13, M14 |

Tools introduced by Track A are listed in [TOOL_CATALOG.md](TOOL_CATALOG.md), including `code.callers`, `context.build` and `code.type_definition`.

## 7. Native inference engine (Track C)

Start with one small **dense decoder-only Transformer**, Safetensors and a known tokenizer. Implement embeddings, RMSNorm, Q/K/V, RoPE, causal attention, residuals, SwiGLU MLP, logits and sampling. Validate FP32 CPU first; add FP16/BF16, quantization and GGUF later. Choose a model whose licence permits the intended use, including any later fine-tuning.

Create both a naive custom Rust CPU tensor backend and a Candle reference backend. Compare intermediate activations and logits within dtype-specific tolerances. Implement a higher-level `InferenceEngine` interface to hide backend-specific tensor types; select an entire backend at model load rather than swapping tensors mid-generation.

```rust
pub trait TensorBackend {
    type Tensor;
    fn matmul(&self, a: &Self::Tensor, b: &Self::Tensor)
        -> Result<Self::Tensor, TensorError>;
    fn rms_norm(&self, x: &Self::Tensor, w: &Self::Tensor, eps: f32)
        -> Result<Self::Tensor, TensorError>;
}

pub trait InferenceEngine: Send + Sync {
    fn capabilities(&self) -> EngineCapabilities;
    // Add async generation, streaming, cancellation and lifecycle.
}
```

Declare backend capabilities explicitly: devices, dtypes, supported shapes, quantization, KV layout, paged attention and batch limits. Add contiguous KV-cache, separate prefill/decode, continuous batching, chunked prefill, block allocator, prefix caching and cancellation in that order. Expose the endpoints from section 4 with Axum; SSE streaming is available to direct clients, while platform-hosted agents use the non-streaming contract until M9.

**Acceptance:** The same model generates correct outputs on custom CPU and Candle; reproducible tests and basic streaming work; the harness completes a recorded task through the OpenAI-compatible endpoint. Measure TTFT, inter-token latency, tokens/s, peak RAM/VRAM and concurrent throughput.

## 8. MLIR / OpenXLA / StableHLO / PJRT (Track C)

The intended split is: **Rust owns API, scheduler, model registry, tensor handles, KV-cache and lifecycle; OpenXLA compiles model graphs; PJRT executes compiled programs on supported devices.** The compiler is an optional execution backend, not the whole serving runtime.

```text
Model weights + configuration
      -> Rust typed Model Graph IR
          |-> Custom CPU executor
          |-> Candle reference adapter
          |-> MLIR lowering bridge (C ABI / cxx)
                  -> StableHLO
                  -> OpenXLA compilation
                  -> PJRT executable
                  -> CPU / supported accelerator
```

**Rust IR MVP:** Parameter, Constant, MatMul, Add, Mul, Reshape, Transpose, Slice, Concat, RMSNorm, RoPE, Softmax, Attention; typed shapes, dtypes and buffer metadata. Start with static shapes and fixed batches; later specialize prefill and decode shape profiles.

**FFI:** MLIR and major OpenXLA components are C++-centric. Use a narrow C ABI or `cxx` boundary, pin toolchain versions and isolate compiler handles. Compile at deployment/model load, **not per token**. Cache compiled executables by model digest, graph version, compiler version, device target, dtype and shape profile.

**Important limitations:** StableHLO does not automatically provide PagedAttention, KV block allocation, continuous batching, prefix caching or specialized quantized attention. These require Rust serving logic, explicit memory control and possibly custom calls/native GPU kernels. Compare compiled and hand-tuned paths empirically.

**Experiments:** (1) Rust IR to StableHLO for matmul + RMSNorm; (2) one fixed-shape Transformer block through PJRT; (3) prefill/decode specialization; (4) graph fusion, memory reuse and compilation cache; (5) custom attention/paged attention boundary; (6) compare OpenXLA/PJRT, Candle and custom CUDA at equal precision.

**Acceptance:** At least one Transformer block compiles and runs through PJRT with verified numerical agreement and measured compilation/runtime trade-offs.

## 9. Distributed KV cache and cost-aware routing (Track C)

Introduce in stages: (1) active VRAM KV and local prefix reuse; (2) RAM/NVMe offload; (3) prefix-aware routing among workers; (4) optional Rust LMCache-like block index and network transfer; (5) optional RustFS cold tier for reusable prefixes. Active attention must read local GPU memory, **not S3**.

Cache identity includes weights revision, tokenizer/token IDs, adapter, positional settings, dtype/layout and tenant boundary. Stage 3 is when the native endpoint's `cache_key` (section 4) becomes necessary. A router compares **warm-worker queue time**, **remote KV transfer time**, and **recompute/prefill time**. Transferring blocks is worthwhile only when cheaper than recomputation or waiting. Begin with TCP; RDMA/GPUDirect is an optional hardware-specific optimization.

All routing and KV logic stays inside Track C. Agents see only the model profile and usage records. Cache entries never cross tenant boundaries.

## 10. Deployment topology

| Mode | Agent and tools (WASI) | Inference | Native jobs | Data |
| --- | --- | --- | --- | --- |
| Developer / feature validation | Native wasm-node running real WASI components | Provider API or explicit local inference endpoint | Deferred; harness-owned disposable service when needed | Checkout-scoped Podman services when needed |
| Advanced local rehearsal | Explicitly provisioned platform microVM testbed, `every_node` placement | Explicit external inference endpoint | Harness-owned service when in scope | Recorded service microVMs or separately approved dependencies |
| Small production | Platform nodes, production-like preset with at least three nodes | Dedicated inference host or provider API over a private network | Dedicated job runner host | Operator-managed PostgreSQL and S3 |
| Scale-out | Platform nodes | Track C router and GPU workers | Job runner pool | Shared storage |

Notes:

- WASI guests cannot use Unix sockets or in-process calls to native services; every off-component call is HTTP or TCP to an explicit endpoint allowed by the app's network profile.
- GPU passthrough in the Firecracker testbed is unverified. Run inference outside the testbed until it is.
- The platform schedules WASI workloads; it does not need Kubernetes. Kubernetes is an option only for the Track C GPU fleet.
- Keep CPU-heavy indexing and compilation off GPU machines. Place code processing near source data and inference near accelerators.
- Local rehearsals follow the repository's testbed skills and canonical scripts; they never target production infrastructure.

## 11. Cross-track security

The harness plan contains the coding agent's threat model, data classes and provider policy. Cross-track rules:

- The platform currently admits only `single-trust-domain` isolation. All agent components on a node share one process, so tenant boundaries are application-level, and only platform-built harnesses and tools are deployed until M17 exists.
- Agents never receive platform control-plane, NATS, Git or provider credentials. Provider keys reach only the model gateway; Git credentials only the job runner (M10 scopes secret references).
- Untrusted native code (build scripts, proc macros, tests) runs only in the job runner, never on platform nodes.
- `local_only` code reaches only self-hosted inference. `confidential` code may also reach an explicitly contracted provider approved by policy, as defined in the harness plan. Track C must enforce tenant separation in caches, logs and retained prompts.
- Track C services are external endpoints: TLS, authentication of the calling gateway, and request size and rate limits are required before any non-loopback deployment.

## 12. Roadmap and delivery gates

### Track B — platform prerequisites

| Item | Needed by | Priority |
| --- | --- | --- |
| External harness-owned native job service (supersedes M19) | A0 full verification, A3 | High |
| M15 object-storage service microVM | A-skel and A1 rehearsal | High |
| M8 configurable request deadline | A1 import, A2 indexing, A3 | High |
| M10 secret reference scoping | Gateway and job runner credentials | Medium |
| M7 service lifecycle | Long-lived gateway or proxy | Medium |
| M13 trace propagation, M14 usage reports | A4 cost evidence | Medium |
| M9 streaming | Streaming user experience | Low; not a blocker |
| M17, M18 | Tenant-uploaded or generated code | Later |

### Track C — inference R&D (parallel, never blocking)

| Phase | Deliverable | Exit gate | Size |
| --- | --- | --- | --- |
| C1 — Inference MVP | Safetensors, custom CPU, Candle, sampling | Numerical differential tests | L |
| C2 — Serving | OpenAI-compatible endpoint, KV cache, batching, cancellation | Harness runs a recorded task; load, latency and memory benchmarks | L |
| C3 — Native endpoint | Idempotency, cancellation, constrained decoding, token usage | Gateway native adapter passes the section 4 acceptance test | M |
| C4 — Compiler R&D | Rust IR → StableHLO → OpenXLA/PJRT | Validated compiled Transformer block | XL |
| C5 — GPU optimization | CUDA, quantization, paged attention | Verified gain at matched correctness | XL |
| C6 — Distribution | Remote workers, model/prefix-aware routing | Multi-node and failure tests | L |
| C7 — Optional KV sharing | Rust KV service, transfer/recompute policy | Positive measured cost or latency result | L |
| C8 — Optional post-training | Verified trajectories from A4, LoRA SFT, subject to tenant permissions | Held-out task improvement at controlled cost | L |

Sizes assume one developer: S ≤ 3 days, M 1–2 weeks, L 3–6 weeks, XL more than 6 weeks. They are rough.

### Track C investment gates

Each Track C phase states its goal in an ADR:

- **Learning goal:** building expertise, with a time box; no cost claim is made.
- **Cost goal:** reducing cost per verified success; requires a break-even estimate before the phase and a measurement after it.

```text
self-hosted cost per million tokens = (amortized hardware + power + operations per hour)
                                      / (measured tokens per hour at realistic utilization / 1e6)
break-even monthly volume           = fixed monthly self-hosted cost
                                      / (provider price per million tokens - variable self-hosted cost per million tokens)
```

Compare at **matched task success**, not matched tokens: a cheaper model that needs more repair attempts can cost more per verified success. Continue a cost-goal phase only if projected volume exceeds break-even or the measured result justifies it.

**Prioritization:** deliver Track A with existing providers first; Track B items in the order above; Track C in parallel, joining Track A only through the model gateway.

## 13. Cross-track benchmarks

The harness plan defines task sourcing, suite hygiene, Track A comparisons and statistics. Cross-track comparisons use the same suites and pins:

1. Provider API versus Track C service through the same gateway contract, at matched task success.
2. Candle versus custom CPU versus CUDA versus OpenXLA at identical precision.
3. Local KV prefix reuse versus cache-aware routing versus remote KV transfer.

Measure verified success, cost per verified success, tokens (including cached), TTFT, ITL, P50/P95 latency, tokens/s, RAM/VRAM, GPU seconds and amortized hardware cost. Report uncertainty and regressions. **A 70% token reduction that lowers task success is not automatically an improvement.**

## 14. Risks and mitigations

| Risk | Mitigation |
| --- | --- |
| No native job runner, so no verified fixes | External job service as a Track A priority; the walking skeleton and A0–A2 remain useful without it |
| Platform limits (memory, deadline) break code tools | Durable bounded activities; M8 |
| Plan documents drift between repositories | Track A detail only in the harness repository; this document only summarizes and links |
| Provider differences behind "OpenAI-compatible" | Internal gateway contract, per-provider adapter tests |
| MLIR/OpenXLA bridge complexity | Narrow pinned FFI; independent fallback backend |
| Compiler lacks efficient paged attention | Native custom kernels and explicit KV block tables |
| Track C never reaches break-even | Investment gates; learning goal declared explicitly |
| Distributed KV transfer exceeds prefill cost | Routing cost model, recompute fallback |
| Multi-tenant cache or data leakage | Tenant namespaces and scoped grants; M17 before untrusted tenants |
| Untrusted build code on platform nodes | Job runner only; no native execution path on nodes |
| Duplicating existing harness work | Build on existing crates; new schema only through migrations |

## 15. ADR backlog

| Repository | ADR |
| --- | --- |
| Harness | See the harness plan (success and cost, workspace, ingestion, tenant model, data policy, gateway contract, index format, integrity, cascade, evaluation) |
| Harness | External native job service: isolation, job API, network, caches, lifecycle |
| Inference | Engine and backend interfaces; native endpoint contract; each Track C phase's goal, break-even estimate and outcome |

## 16. Platform and inference tasks

Track A tasks, with sizes and dependencies, are in the harness plan.

| # | Task | Track | Size |
| --- | --- | --- | --- |
| 1 | External job service ADR: runner shape, job API, isolation, caches | A | S |
| 2 | Native job image, lifecycle and validation through agentic-dev jobs | A | L |
| 3 | M15 object-storage service microVM | B | M |
| 4 | M8 configurable request deadline | B | M |
| 5 | M10 secret reference scoping | B | M |
| 6 | M7, M13, M14 as Track A reaches A3–A4 | B | L |
| 7 | Create the inference repository, CI, ADR template, pinned toolchain; record the C1 goal | C | S |
| 8 | Tensor metadata and custom CPU backend; Candle reference backend with differential tests | C | L |
| 9 | Safetensors loader, tokenizer, one dense forward pass, greedy and stochastic sampling | C | L |
| 10 | Contiguous KV cache, prefill/decode split, OpenAI-compatible endpoint | C | M |
| 11 | Scheduler, cancellation and continuous batching; run a recorded harness task | C | L |
| 12 | Break-even estimate before C3 or later cost-goal phases | C | S |
| 13 | Native endpoint fields as benchmarks justify them | C | M |
| 14 | Typed graph IR, MLIR/StableHLO proof of concept, fixed-shape PJRT execution | C | XL |
| 15 | Backend benchmarks and shape-specific artifact cache; choose CUDA, paged attention or distributed cache from measured bottlenecks | C | M |

## 17. Definition of done

**First useful release (Track A with Track B prerequisites).** A tenant's Rust repository is imported, a model allowed for its data class proposes a fix through the gateway, the job runner compiles and tests the patch, independent verification passes, a replayable history is stored, and the patch is delivered as an artifact or, after approval, a pull request. Cost per verified success is compared against the baseline on a frozen held-out suite. The full definition is in the harness plan.

**First inference milestone (Track C, separate).** One model runs on both Candle and the custom CPU backend with verified numerical agreement, is served through the OpenAI-compatible endpoint, and completes a recorded harness task selected only by model-profile configuration. MLIR/OpenXLA remains a measurable optional backend track.

**Long-term outcome:** A portable, cost-aware agent platform whose WASI tools, code intelligence, validation harness, Rust inference engine and compiler backends evolve independently behind stable contracts.

## 18. Open questions

Track A questions are in the harness plan.

1. Which job runner shape comes first: one long-lived runner microVM with per-job disposable environments, or a fresh microVM per job? This trades start-up time against isolation strength.
2. Which registry mirror or vendoring approach supplies dependencies to offline jobs?
3. What name and ownership does the Track C repository have, and when do sections 7–9 move there?
4. Is GPU passthrough feasible in the local testbed, or does Track C always run outside it?
5. How does the gateway authenticate to a non-loopback Track C endpoint?

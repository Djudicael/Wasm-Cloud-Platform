# Step 38 — Self-Hosted Open-Source LLM Inference: Feasibility Study & Architecture

## Goal

Determine whether the Wasm Cloud Platform can be adapted to serve **self-hosted open-source
LLM inference** (Llama, Mistral, Qwen, Phi, etc.), eliminating per-token API costs from
providers like OpenAI, Anthropic, or Together AI.

## Conclusion: YES — It Is Completely Possible

The Wasm Cloud Platform **can** serve self-hosted open-source LLMs. The architecture is
straightforward, the changes are bounded, and the economics strongly justify it. The key
insight is: **the platform manages llama.cpp, it does not reimplement inference.**

llama.cpp already solves the hard problems — quantization, CPU offloading, multi-GPU,
KV cache management, and LoRA adapters. The platform wraps it with the operational
layer that standalone inference servers lack: multi-tenant isolation, per-tenant billing,
request routing, secret management, health monitoring, and auto-scaling.

---

## Context & Rationale

### The Problem This Solves

Running LLMs through API providers is expensive and creates hard dependencies:

| Provider | Model | Input Cost / 1M tokens | Output Cost / 1M tokens |
|----------|-------|----------------------|-------------------------|
| OpenAI | GPT-4o | $2.50 | $10.00 |
| OpenAI | GPT-4o-mini | $0.15 | $0.60 |
| Anthropic | Claude 3.5 Sonnet | $3.00 | $15.00 |
| Together | Llama 3.1 70B | $0.80 | $0.80 |
| **Self-hosted** | **Llama 3.1 8B Q4** | **~$0.02** | **~$0.02** |
| **Self-hosted** | **Llama 3.1 70B Q4** | **~$0.10** | **~$0.10** |

Self-hosting open-source models on owned hardware is **10–150× cheaper** at volume.

But self-hosting requires infrastructure: model serving, request routing, multi-tenancy,
billing, monitoring, and scaling. **The Wasm Cloud Platform already provides most of this.**
The question is: can it be adapted for LLM workloads without breaking what already works?

### Why This Platform (Not vLLM, Not Ollama, Not TGI)

Existing LLM serving frameworks solve inference but not platform operations:

| Concern | vLLM | Ollama | TGI | **Wasm Cloud (adapted)** |
|---------|------|--------|-----|--------------------------|
| Multi-tenant isolation | ❌ shared process | ❌ single user | ❌ shared process | ✅ WASM sandbox per tenant |
| Per-tenant billing | ❌ | ❌ | ❌ | ✅ fuel + token accounting |
| Dynamic routing | ❌ | ❌ | limited | ✅ Pingora + route tables |
| Secret management | ❌ | ❌ | ❌ | ✅ DEK/KEK hierarchy |
| Hot-swap deploy | ❌ restart | ❌ restart | ❌ restart | ✅ blue-green drain |
| Auto-scaling | basic | ❌ | basic | ✅ fuel-based + concurrency |
| Control plane | ❌ | ❌ | ❌ | ✅ NATS pub/sub |
| Observability | basic | ❌ | basic | ✅ Prometheus + structured logs |
| LoRA adapter mgmt | ✅ | ❌ | ✅ | ✅ artifact system (adapted) |
| GPU sharing | ✅ | limited | ✅ | ✅ via llama.cpp + WASI-NN |

The platform's operational layer is a **genuine advantage**. The gap is inference
capability — and that gap is closed by integrating llama.cpp as a native backend.

### Why Open-Source Models Specifically

Open-source models are the only models you can legally self-host. They are also
increasingly competitive with proprietary models:

- **Llama 3.1 70B** approaches GPT-4 class on many benchmarks
- **Llama 3.1 8B** outperforms GPT-3.5-turbo on most tasks
- **Phi-3-mini** runs on CPU at usable speeds
- **Qwen 2.5** series covers 0.5B–72B with strong multilingual support
- **Mistral 7B** is fast and efficient for its size

The open-source ecosystem also enables **LoRA fine-tuning** — adapting a base model to
your domain without retraining the full model. This is the architecture described in
`STUDIES/wasm_lo_ra_fine_tuning_architecture_edge_ai.md`.

---

## 1. How "Models Too Big for Memory" Is Actually Solved

The STUDIES document `wasm_llm_streaming_architecture_guide_english.md` describes layer
streaming as a technique for running models on RAM-constrained devices. This is a real
technique, but it is **not how LLM providers solve the memory problem in production.**
Here is what actually works.

### 1.1 Quantization — The Single Biggest Win

You do not run FP16 models in production. Quantization reduces model size with negligible
quality loss:

```
Llama 3.1 70B:
  FP16 (original):     140 GB  ← needs 2× A100 80GB
  INT8:                70 GB   ← needs 1× A100 80GB
  Q4_K_M (4-bit):     38 GB   ← needs 1× A100 80GB with room for KV cache
  Q3_K_M (3-bit):     30 GB   ← fits on 1× A100 80GB comfortably

Llama 3.1 8B:
  FP16:               16 GB   ← needs 1× A100 or 2× consumer GPUs
  Q4_K_M:              4.5 GB ← fits on ANY modern GPU
  Q3_K_M:              3.5 GB ← fits on a 6GB GPU
```

Quality impact of Q4_K_M vs FP16: **less than 1% on most benchmarks.** You cannot tell
the difference in chat. This is the first and most important answer to "models are too
big for memory."

### 1.2 CPU Offloading — Use RAM You Already Have

llama.cpp has a built-in feature: put some layers on GPU, the rest on CPU RAM. This
means you do not need the entire model in GPU memory:

```
Machine: 1× RTX 3090 (24GB VRAM) + 64GB system RAM

Llama 3.1 70B Q4 (38 GB total):
  GPU VRAM (24 GB):  20 layers (~20 GB) + KV cache (~2 GB)
  System RAM (64 GB): 12 remaining layers (~12 GB)
  Speed: ~15-20 tokens/sec
  ✅ Works. Usable for chat. No second GPU needed.

Machine: No GPU at all, 32GB RAM

Llama 3.1 8B Q4 (4.5 GB total):
  System RAM: all 32 layers (~4.5 GB) + KV cache (~0.5 GB)
  Speed: ~10-15 tokens/sec
  ✅ Works. No GPU at all. Usable for chat.
```

**This is how individuals self-host 70B models on a single machine.** You do not need
the entire model in GPU memory. CPU offloading is the bridge.

### 1.3 GQA Models — Smaller KV Cache by Design

Newer open-source models use **Grouped-Query Attention (GQA)**, which shrinks the KV
cache by sharing key/value heads across multiple query heads:

```
Model                    | Attention | KV cache (4K ctx) | KV cache (32K ctx)
-------------------------|-----------|-------------------|-------------------
Llama 2 7B (old MHA)    | 32 heads  | ~2.0 GB           | ~16 GB
Llama 3.1 8B (GQA)      | 8 KV heads| ~0.5 GB           | ~4 GB
Mistral 7B (GQA)        | 8 KV heads| ~0.5 GB           | ~4 GB
Qwen 2.5 7B (GQA)       | 4 KV heads| ~0.25 GB          | ~2 GB
```

**Llama 3.1 8B with GQA + INT4 KV cache:**

```
Model weights (Q4):  4.5 GB
KV cache (4K ctx):   0.125 GB   ← was 2 GB with old MHA, now 0.125 GB
Total on GPU:        ~4.6 GB

Fits on a single 8GB GPU with room to spare ✅
```

### 1.4 KV Cache Quantization

llama.cpp supports quantizing the KV cache independently of the model weights:

```
FP16 KV cache (default):  ~2 GB for 4K context
INT8 KV cache:            ~1 GB  (2× reduction, ~0.1% quality loss)
INT4 KV cache:            ~0.5 GB (4× reduction, ~0.5% quality loss)

Enabled with: --cache-type q4_0 in llama.cpp
```

### 1.5 Multi-GPU (Tensor Parallelism)

When one GPU is not enough, llama.cpp splits the model across multiple GPUs:

```
Llama 3.1 70B Q4 (38 GB) on 2× RTX 3090 (24GB each):
  GPU 0: layers 0-15  (~19 GB)
  GPU 1: layers 16-31 (~19 GB)
  Speed: ~30-40 tokens/sec
  ✅ Full GPU speed, model split across GPUs
```

### 1.6 What About Layer Streaming from the STUDIES?

The STUDIES document describes loading model layers one at a time from disk to save RAM.
This is a real technique for **extremely constrained devices** (embedded, IoT), but it
has a critical limitation for interactive use:

```
Llama 8B = 32 layers, each ~150 MB

Per token generated:
  32 layers × 150 MB = 4.8 GB of disk reads
  At 30 tokens/sec: 144 GB/sec of disk I/O
  NVMe max sequential read: ~7 GB/sec

  → 20× too slow for real-time generation
```

Layer streaming is useful for batch processing where latency does not matter. For
interactive chat, **quantization + CPU offloading** is the production solution. llama.cpp
handles both automatically — the platform does not need to reimplement this.

### 1.7 What About llama.cpp Compiled to WASM?

llama.cpp has a WASM build target (emscripten). It compiles and runs. But:

- **CPU only** — no GPU access from WASM
- **4GB memory limit** — same WASM linear memory wall
- **No SIMD optimization** — WASM SIMD is limited
- **Speed: ~1-3 tokens/sec** for 7B model on CPU

It is a **demo**, not production infrastructure. The correct approach is running llama.cpp
as a **native backend** and accessing it from WASM via WASI-NN host functions.

### 1.8 Summary: What Fits Where

| Model | Quantized Size | Hardware Needed | Distributed? |
|-------|---------------|-----------------|-------------|
| Phi-3-mini 3.8B | 2 GB | Any laptop | ❌ No |
| Llama 3.1 8B | 4.5 GB | Any GPU or 8GB RAM | ❌ No |
| Mistral 7B | 4 GB | Any GPU or 8GB RAM | ❌ No |
| Llama 3.1 70B | 38 GB | 1× A100 or 2× RTX 3090 or CPU offload | ❌ No |
| Llama 3.1 405B | 220 GB | 4× A100 or multi-machine | ⚠️ Maybe |
| DeepSeek 671B | 370 GB | 8× A100 or multi-machine | ✅ Yes |

**For 95% of self-hosted use cases, you need ONE machine.** Quantization + CPU offloading
solves the memory problem. The platform does not need to solve distributed inference.

---

## 2. The Architecture: WASM Orchestrates, llama.cpp Computes

### 2.1 Core Principle

The LLM inference engine runs as a **native process** (llama.cpp) managed by the platform.
WASM modules handle the orchestration layer: request routing, auth, token counting,
rate limiting, billing, and LoRA adapter management. The WASI-NN interface is the bridge.

```
External Request (POST /v1/chat/completions)
       │
       ▼
   Pingora Proxy ──────────────────────────────────────
       │  TLS termination, rate limiting, routing
       │
       ▼
   WASM LLM Gateway Module (sandboxed, per-tenant, ~100KB)
       │
       │  ┌──────────────────────────────────────────┐
       │  │  Inside WASM sandbox:                     │
       │  │  - Validate request (API key, model name) │
       │  │  - Apply tenant quotas (tokens/day)       │
       │  │  - Call wasi-nn inference function         │
       │  │  - Stream response tokens back             │
       │  │  - Record token counts for billing         │
       │  └──────────────────────────────────────────┘
       │
       ▼ (WASI-NN call crosses sandbox boundary)
       │
   Native Backend: llama.cpp (NOT in WASM, NOT sandboxed)
       │  ┌──────────────────────────────────────────┐
       │  │  - Model weights mmap'd from disk          │
       │  │  - Quantization: Q4_K_M, Q3_K_M, etc.     │
       │  │  - GPU execution (CUDA / Metal / ROCm)    │
       │  │  - CPU offloading for layers that don't    │
       │  │    fit on GPU                              │
       │  │  - KV cache in host RAM (quantized)        │
       │  │  - Paged attention memory management      │
       │  │  - LoRA adapter hot-loading               │
       │  └──────────────────────────────────────────┘
       │
       ▼
   Response Stream (SSE: token-by-token)
       │
       ▼
   Pingora Proxy ──── Client
```

### 2.2 Why This Works

| Concern | Where It Lives | Why |
|---------|---------------|-----|
| Request validation | WASM sandbox | Tenant isolation, quota enforcement |
| API key auth | WASM sandbox | Per-tenant secret in encrypted store |
| Token counting | WASM sandbox | Billing integrity (sandbox cannot lie) |
| Inference compute | llama.cpp (native) | GPU access, mmap, full RAM, quantization |
| Model weights | Host filesystem | Too large for WASM memory, mmap'd by llama.cpp |
| KV cache | Host RAM + redb (cold) | Hot in RAM, cold pages in local redb |
| LoRA adapters | Host filesystem | Loaded/unloaded by llama.cpp on supervisor command |
| Rate limiting | Pingora + WASM | Two layers of defense |
| Billing records | redb (via billing crate) | Hash chain, same as today |
| Model distribution | Artifact server + HTTP | Base models via HTTP, adapters via artifact server |

### 2.3 What the WASM Module Actually Does

The WASM LLM gateway module is **small** — it is an orchestration shim, not the model:

```rust
// Pseudocode for the WASM LLM gateway module
fn handle_chat_completion(request: ChatRequest) -> Stream<Token> {
    // 1. Validate tenant has access to requested model
    let model = validate_model_access(request.model, tenant_id)?;

    // 2. Check token quota
    check_token_quota(tenant_id, request.max_tokens)?;

    // 3. Call WASI-NN inference (crosses sandbox boundary to llama.cpp)
    let stream = wasi_nn::infer_stream(model, request.messages, request.params)?;

    // 4. Count tokens for billing
    let (input_tokens, output_tokens) = count_tokens(request, stream);

    // 5. Record billing (via host function)
    host::record_llm_usage(tenant_id, model, input_tokens, output_tokens);

    // 6. Stream tokens back
    stream
}
```

This module is ~100KB of WASM, not 4GB. It cold-starts in <10ms. The heavy compute
happens in llama.cpp, which is already running with the model loaded.

### 2.4 The Native Backend: llama.cpp

llama.cpp is the recommended backend because it already handles everything the platform
would otherwise need to build:

| Feature | llama.cpp Support | Platform Needs to Build? |
|---------|-------------------|--------------------------|
| GGUF quantization (Q2–Q8) | ✅ Built-in | ❌ No |
| CPU offloading (partial GPU) | ✅ `--n-gpu-layers` | ❌ No |
| Multi-GPU (tensor parallel) | ✅ Built-in | ❌ No |
| KV cache quantization | ✅ `--cache-type` | ❌ No |
| Paged attention | ✅ Built-in | ❌ No |
| LoRA adapter loading | ✅ `--lora` | ❌ No |
| Multi-user batching | ✅ Continuous batching | ❌ No |
| OpenAI-compatible API | ✅ Built-in server | ❌ No (but we use WASI-NN instead) |
| mmap model loading | ✅ Built-in | ❌ No |

**The platform's job is operational intelligence, not reimplementing inference.**

---

## 3. KV Cache Architecture

### 3.1 Why Remote KV Cache Does Not Work for Real-Time Inference

The KV cache cannot be stored on a remote machine for real-time inference. The reason
is fundamental to how transformers work: **every token requires attention over ALL
previous tokens.**

```
Generating token #4000:
  Query:    token #4000's query vector     (tiny, ~16 KB)
  Keys:     ALL 3999 previous key vectors  (THE ENTIRE KV CACHE)
  Values:   ALL 3999 previous value vectors (THE ENTIRE KV CACHE)

  Attention = softmax(Q × K^T) × V
                      ↑           ↑
              needs ALL keys   needs ALL values
```

Every single token requires a full scan of the KV cache. This is not a random lookup —
it is a sequential scan of everything.

```
Llama 8B, 4K context, FP16 KV cache:
  KV cache size: ~2 GB
  Network (10 Gbps datacenter): ~1.25 GB/sec
  Read 2 GB over network: ~1.6 seconds

  Per token: 1.6 seconds just for KV cache reads
  Native GPU inference: 0.012 seconds per token (80 tok/s)

  Remote KV cache is 130× SLOWER
```

Even with the fastest networks:

| Network | Read 2GB | Tokens/sec | vs Local RAM |
|---------|----------|------------|-------------|
| 10 Gb datacenter | 1.6s | 0.6 | 130× slower |
| 100 Gb datacenter | 0.16s | 6 | 13× slower |
| InfiniBand 400 Gb | 0.04s | 25 | 3× slower |
| **Local RAM** | **0.01s** | **80** | **1×** |

**The KV cache must be local for real-time inference.**

### 3.2 What DOES Work: Local redb as Cold KV Tier

The hybrid cache architecture from the STUDIES document works when the cold tier is
**local disk, not network**:

```
GPU VRAM (hot):
  - Active session KV cache
  - Model weights (GPU layers)

Host RAM (warm):
  - Paged KV cache for idle sessions
  - Model weights (CPU offloaded layers)

redb on LOCAL NVMe (cold):
  - Evicted KV cache pages for paused sessions
  - Latency: ~0.1ms (local disk), not ~1.6s (network)
```

When a session goes idle, its KV cache is evicted from GPU → RAM → redb. When the user
comes back, it is loaded back. This is **session persistence**, not real-time access.

redb's MVCC means reading a cold KV cache page does not block the writer (billing
records being inserted). The key schema:

```
Key:   session_id (UUID string)
Value: serialized KV cache pages (1–50 MB per session)
Access: read-heavy, write-on-exit, TTL-based expiry
```

### 3.3 Remote KV Cache for Disaster Recovery (Not Real-Time)

A separate machine CAN store KV cache snapshots for non-real-time purposes:

```
Node A (Inference):
  - Model weights in GPU VRAM
  - Hot KV cache in GPU VRAM / host RAM
  - Generates tokens at 80 tok/s

Node B (KV Persistence):
  - redb / Redis / PostgreSQL
  - Stores KV cache snapshots for:
    ✅ Session migration (Node A dies, Node C picks up)
    ✅ Session resumption (user returns after 1 hour)
    ✅ System prompt prefix cache (shared across nodes)
    ❌ NOT for real-time attention computation
```

The NATS control plane already connects these nodes. When Node A crashes, NATS health
events (Step 37) trigger Node C to load the KV cache from Node B and resume sessions.

---

## 4. What the Platform Can Do Today (No Changes)

These capabilities work as-is and are directly useful for LLM serving:

### 4.1 Pingora Proxy (North-South)

- TLS termination for HTTPS endpoints
- HTTP/1.1 and HTTP/2 proxying
- Dynamic upstream table (add/remove instances without restart)
- Rate limiting (token bucket, per-tenant and per-IP)
- Request routing via host header

### 4.2 NATS Control Plane

- Deploy/remove events across nodes
- Secret rotation propagation
- Cluster state sync via JetStream replay

### 4.3 Billing System

- Per-instance fuel accounting with hash-chain tamper evidence
- Per-tenant billing reports
- S3 export for invoicing

### 4.4 Artifact Server

- HTTP artifact server on port 9091 for binaries >1MB
- SHA-256 hash verification (once P1 fix from Step 31 is implemented)

### 4.5 Storage (redb)

- MVCC concurrent reads/writes
- Typed tables with generic keys
- Schema versioning and migration

### 4.6 WASI Policy Enforcement (Step 33, once implemented)

- Per-app network policies (outbound CIDR restrictions)
- Per-app filesystem policies (allowed paths, write limits)
- Per-app FD limits

---

## 5. What Must Change in Each Layer

### 5.1 `crates/runtime` — WASI-NN Integration (Major)

**Current state**: Wasmtime with WASI Preview 2 only. No WASI-NN. No GPU awareness.
Memory limit is `MemoryPages(2048)` = 128MB per instance.

| Change | Effort | Priority |
|--------|--------|----------|
| Add `wasmtime-wasi-nn` crate dependency | S | P0 |
| Register WASI-NN in the linker alongside WASI Preview 2 | S | P0 |
| New `LlmInstanceConfig` alongside `AppConfig` | M | P0 |
| Remove memory limit for LLM gateway modules (they are small) | S | P1 |
| Add host function `host::record_llm_usage()` for billing | M | P1 |
| Add host function `host::get_kv_cache_page()` for externalized cache | M | P2 |
| Add host function `host::list_available_models()` | S | P2 |

**Key design decision**: The WASI-NN backend is selected at the **node level**, not the
app level. A node with CUDA GPUs uses the CUDA backend. A node without GPUs uses the
CPU backend. The WASM module is portable — it just calls `wasi-nn::infer()`.

**WASI-NN backend options**:

| Backend | GPU | CPU | LoRA | Paged Attn | Maturity |
|---------|-----|-----|------|------------|----------|
| llama.cpp (GGUF) | ✅ CUDA/ROCm/Metal | ✅ | ✅ | ✅ | Production |
| candle (HuggingFace) | ✅ CUDA | ✅ | ⚠️ | ❌ | Stable |
| ONNX Runtime | ✅ CUDA | ✅ | ❌ | ❌ | Production |

**Recommendation**: Start with **llama.cpp** as the WASI-NN backend. It supports GGUF
quantization, LoRA adapters, paged attention, CPU offloading, and runs on CPU + GPU.

### 5.2 `crates/supervisor` — LLM Instance Lifecycle (Major)

**Current state**: Instances are spawned on demand, killed when idle, and follow a
short-lived HTTP request/response pattern. `idle_timeout_secs: 300` is the default.

| Change | Effort | Priority |
|--------|--------|----------|
| New `InstanceType` enum: `Http` vs `Llm` | S | P0 |
| LLM instances are **long-lived** (hours/days, not minutes) | S | P0 |
| LLM instances have a **warm pool** (pre-loaded models) | M | P1 |
| Different health check for LLM (GPU memory, not HTTP 200) | M | P1 |
| Model loading coordination (one native backend per GPU) | L | P1 |
| LoRA adapter hot-loading/unloading via supervisor commands | M | P2 |
| KV cache lifecycle management (expire cold sessions) | M | P2 |

**Key design decision**: The native inference backend (llama.cpp) runs as a **singleton
per GPU**. Multiple WASM gateway modules share it via WASI-NN calls. The supervisor
manages this shared resource:

```
Node with 1 GPU:
  ┌─────────────────────────────────┐
  │  Native Backend (llama.cpp)     │
  │  - Model: Llama-3.1-8B-Q4      │
  │  - GPU 0: 8GB VRAM              │
  │  - Active LoRA: tenant-A, B    │
  ├─────────────────────────────────┤
  │  WASM Gateway: tenant-A         │──→ wasi-nn::infer("tenant-A-lora", ...)
  │  WASM Gateway: tenant-B         │──→ wasi-nn::infer("tenant-B-lora", ...)
  │  WASM Gateway: tenant-C (base)  │──→ wasi-nn::infer("base", ...)
  └─────────────────────────────────┘
```

### 5.3 `crates/storage` — Model Metadata & KV Cache (Moderate)

**Current state**: redb stores artifacts, configs, secrets, billing records, and metrics.
Artifacts are WASM binaries (typically <10MB).

| Change | Effort | Priority |
|--------|--------|----------|
| New `MODEL_REGISTRY` table (model_id → metadata) | S | P0 |
| New `KV_CACHE` table (session_id → cache pages) | M | P1 |
| New `TOKEN_QUOTA` table (tenant_id → daily/monthly limits) | S | P1 |
| New `LLM_USAGE` table (tenant_id → token consumption) | S | P1 |
| Model weight files stored on **filesystem**, NOT in redb | S | P0 |
| Schema migration for new tables (Step 22 extension) | S | P1 |

**Why model weights are NOT in redb**: redb is an embedded KV store optimized for
small-to-medium values. A 4GB model weight file would bloat the redb file, cause
excessive write amplification, and make MVCC snapshots expensive. Model weights are
**immutable large files** — they belong on the filesystem with SHA-256 hash
verification, managed by the artifact server.

**Model storage layout**:

```
/data/wasm-cloud/models/
├── llama-3.1-8b-q4/
│   ├── model.gguf              (4.5 GB, immutable)
│   ├── model.gguf.sha256       (64-char hex digest)
│   └── meta.json               (context_length, tokenizer info)
├── llama-3.1-70b-q4/
│   ├── model.gguf
│   ├── model.gguf.sha256
│   └── meta.json
└── lora-adapters/
    ├── tenant-a-law/
    │   ├── adapter.bin          (50 MB)
    │   └── adapter.bin.sha256
    └── tenant-b-medical/
        ├── adapter.bin          (80 MB)
        └── adapter.bin.sha256
```

### 5.4 `crates/proxy` — SSE Streaming & Long Connections (Moderate)

**Current state**: Pingora proxies HTTP request/response. Timeouts are configured for
short-lived connections (typical API calls <1s).

| Change | Effort | Priority |
|--------|--------|----------|
| SSE (Server-Sent Events) response streaming | M | P0 |
| Longer idle timeouts for streaming connections (60s+) | S | P0 |
| Request queuing when all GPU slots are busy | M | P1 |
| Per-model routing (route to node with model loaded) | M | P1 |
| Backpressure: return 503 when GPU queue is full | S | P1 |

**Key design decision**: LLM streaming responses use `text/event-stream`. Pingora must
not buffer the entire response before forwarding. This requires configuring Pingora's
response buffering to **pass-through mode** for streaming endpoints.

**Request queuing**: Unlike HTTP apps (where you spawn another instance), LLM inference
is bounded by GPU count. When all GPU slots are busy, requests must queue. The proxy
should:
1. Return `503 Service Unavailable` with `Retry-After` header if queue is full
2. Return `202 Accepted` with a queue position if queue has room
3. Use SSE to stream the response once inference begins

### 5.5 `crates/billing` — Token-Based Accounting (Moderate)

**Current state**: Billing is fuel-based. `BillingRecord` has `fuel_consumed` and
`fuel_quota`. Reports aggregate fuel per tenant.

| Change | Effort | Priority |
|--------|--------|----------|
| New `LlmBillingRecord` alongside `BillingRecord` | M | P0 |
| Fields: `input_tokens`, `output_tokens`, `model_id`, `lora_adapter` | S | P0 |
| Token-based billing report alongside fuel-based report | M | P1 |
| Hash chain covers token counts (tamper-evident) | S | P0 |
| Cost calculation: tokens × model_price_per_1k | S | P2 |

**Why a separate record type**: Fuel and tokens measure fundamentally different things.
Fuel measures WASM instructions (useful for HTTP apps). Tokens measure LLM output
(useful for LLM apps). Mixing them in one record would confuse reporting. Both record
types share the same hash chain mechanism and redb persistence.

### 5.6 `crates/common` — New LLM Types (Minor)

| Change | Effort | Priority |
|--------|--------|----------|
| New `ModelId(String)` newtype with validation | S | P0 |
| New `TokenCount(u64)` newtype | S | P0 |
| New `LlmAppConfig` alongside `AppConfig` | M | P0 |
| New `InstanceType` enum: `Http`, `Llm` | S | P0 |
| New `LlmBillingRecord` struct | M | P0 |

**`LlmAppConfig`**:

```rust
pub struct LlmAppConfig {
    pub app_id: AppId,
    pub model_id: ModelId,
    pub lora_adapter: Option<String>,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
    pub temperature: f32,
    pub top_p: f32,
    pub daily_token_quota: u64,
}
```

### 5.7 `crates/messaging` — Model Distribution Events (Minor)

| Change | Effort | Priority |
|--------|--------|----------|
| New `ModelDeployed` event | S | P1 |
| New `ModelRemoved` event | S | P1 |
| New `LoRAAdapterUpdated` event | S | P2 |
| New `KVCacheInvalidate` event | S | P2 |
| JetStream durability for model events (not ephemeral) | S | P1 |

Model deployment events must be durable (JetStream) because downloading a 4GB model
takes minutes. If a node misses the event, it must be able to replay it.

### 5.8 `crates/node` — GPU Detection & Model Loading (Moderate)

| Change | Effort | Priority |
|--------|--------|----------|
| GPU detection at startup (CUDA device count, VRAM) | M | P0 |
| Report GPU capabilities via NATS health event | S | P1 |
| Model download from HTTP source (HuggingFace, S3) | M | P0 |
| Model SHA-256 verification after download | S | P0 |
| Native backend process management (start/stop llama.cpp) | M | P1 |
| Model pre-loading based on node config | S | P1 |

### 5.9 `crates/ctl` — LLM CLI Commands (Minor)

| Change | Effort | Priority |
|--------|--------|----------|
| `wasm-ctl model list` — show available models | S | P1 |
| `wasm-ctl model deploy --model llama-3.1-8b --source huggingface://...` | M | P1 |
| `wasm-ctl model remove --model llama-3.1-8b` | S | P1 |
| `wasm-ctl lora deploy --app tenant-a --adapter ./adapter.bin` | M | P2 |
| `wasm-ctl lora remove --app tenant-a` | S | P2 |
| `wasm-ctl llm status` — show GPU usage, queue depth, active models | M | P1 |

### 5.10 What Does NOT Need to Change

| Component | Why No Change Needed |
|-----------|---------------------|
| `crates/secrets` | API keys for LLM endpoints stored in encrypted secrets |
| `crates/metrics` | Prometheus exporter works for LLM metrics (add new counters) |
| NATS core messaging | Same pub/sub infrastructure, just new event types |
| Pingora core | Same proxy, just SSE pass-through mode |
| redb core | Same database, just new tables |
| Billing hash chain | Same mechanism, extended with token fields |
| Artifact server | Same HTTP server, used for LoRA adapter distribution |
| DNS integration | Same wildcard domain routing |
| Graceful shutdown | Same drain protocol for LLM instances (finish active requests) |

---

## 6. Configuration: New `llm` Section in `node.toml`

Extending the configuration defined in Step 32:

```toml
[llm]
enabled = true
models_dir = "/data/wasm-cloud/models"
backend = "llama-cpp"           # llama-cpp | candle | onnx-runtime
gpu_device = 0                  # CUDA device index (-1 for CPU only)
max_gpu_memory_mb = 8192        # Max VRAM to use
kv_cache_enabled = true
kv_cache_ttl_secs = 3600        # Cold KV cache expiry
kv_cache_max_sessions = 1000    # Max sessions in cold cache

[llm.models."llama-3.1-8b-q4"]
source = "huggingface://meta-llama/Llama-3.1-8B-Instruct-GGUF/Q4_K_M"
file = "llama-3.1-8b-q4/model.gguf"
context_length = 8192
max_output_tokens = 4096
gpu_layers = 32                 # All layers on GPU (fits in 8GB+)
input_price_per_1k_tokens = 0.00001
output_price_per_1k_tokens = 0.00002

[llm.models."llama-3.1-70b-q4"]
source = "huggingface://meta-llama/Llama-3.1-70B-Instruct-GGUF/Q4_K_M"
file = "llama-3.1-70b-q4/model.gguf"
context_length = 8192
max_output_tokens = 4096
gpu_layers = 20                 # Partial GPU offloading (rest on CPU)
input_price_per_1k_tokens = 0.00005
output_price_per_1k_tokens = 0.00010

[llm.queues]
max_depth = 100                 # Max requests waiting for GPU
timeout_secs = 300              # Max wait time in queue
priority_levels = 3             # Low, medium, high priority queues
```

---

## 7. Relationship to INFRA_IMPL 31–37

The improvements already planned in Steps 31–37 are **prerequisites** for LLM support.

### 7.1 Critical Prerequisites (Must Be Done First)

| Step | Title | Why It Is Critical for LLM |
|------|-------|----------------------------|
| 31 | Project Analysis & Improvements | P1 fixes (SHA-256 verification, `db_path` fix) are needed for model integrity |
| 32 | Configuration Management | The `[llm]` section in `node.toml` requires the config system from Step 32 |
| 33 | WASI Policy Enforcement | LLM instances need a distinct policy profile (high memory, GPU access) |
| 34 | Admin API Security | LLM endpoints must be authenticated — token quotas are a billing surface |
| 37 | Health Check Protocol | GPU health (VRAM usage, temperature) must be reported for LLM routing |

### 7.2 Important but Not Blocking

| Step | Title | Why It Helps |
|------|-------|-------------|
| 35 | Chaos Testing | GPU failure, OOM, model corruption are new failure modes to test |
| 36 | Structured Logging | LLM inference logs (token throughput, queue depth) need structured format |

### 7.3 New Documents Needed

| # | Document | Priority | Description |
|---|----------|----------|-------------|
| 39 | WASI-NN Integration | P0 | How wasmtime-wasi-nn is linked, backend selection, GPU management |
| 40 | LLM Instance Lifecycle | P0 | Warm pools, model loading, LoRA hot-swap, KV cache management |
| 41 | Model Distribution | P1 | Download, verify, store, and serve model weight files |
| 42 | Token-Based Billing | P1 | LlmBillingRecord, token quotas, pricing model, hash chain extension |
| 43 | SSE Streaming Protocol | P1 | Pingora pass-through, request queuing, backpressure signaling |

---

## 8. LoRA Fine-Tuning Integration

The `STUDIES/wasm_lo_ra_fine_tuning_architecture_edge_ai.md` document describes the
architecture correctly. The platform can support it with these additions:

### 8.1 What the Platform Does

- **LoRA adapter storage**: Use the existing artifact system (adapters are 10–200MB)
- **LoRA adapter distribution**: NATS events + artifact server
- **LoRA hot-loading**: Supervisor commands llama.cpp to load/unload adapters
- **Per-tenant LoRA**: Each tenant's WASM gateway specifies which adapter to use

### 8.2 What the Platform Does NOT Do

- **LoRA training**: NOT done on the platform. Requires GPU training infrastructure
  (Python + PyTorch + cloud GPU). Pre-trained adapters are uploaded via
  `wasm-ctl lora deploy`.

### 8.3 The Real Cost Saving: LoRA Eliminates the Need for Big Models

The biggest cost saving is not per-token pricing — it is **not needing GPT-4 at all**.
A LoRA-adapted 8B model on your specific domain can match GPT-4 quality for your
use case at 1/100th the cost:

- **Base model**: Llama 3.1 8B (free, open-source)
- **LoRA adapter**: Trained on your data (one-time cost: ~$5–20 on cloud GPU)
- **Serving cost**: Same as base model (LoRA adds ~10% inference overhead)
- **Quality**: Domain-specific, often better than GPT-4 for your use case

---

## 9. Cost Model: When Does Self-Hosting Win?

### 9.1 Break-Even Analysis

| Setup | Fixed Cost/Month | Variable Cost/1M tokens | Break-Even vs GPT-4o-mini |
|-------|-----------------|------------------------|--------------------------|
| 1× A10G (8B model) | $200 | ~$0.02 | >13M tokens/month |
| 1× A100 (70B model) | $1,100 | ~$0.10 | >183M tokens/month |
| 3× A10G (8B, HA) | $600 | ~$0.02 | >40M tokens/month |
| On-prem GPU (owned) | $0 (sunk) | ~$0.001 (electricity) | Immediately |

**Key insight**: If you already own GPU hardware (gaming rig, workstation, or on-prem
server), self-hosting is **immediately cheaper** than any API provider.

### 9.2 Scale Comparison

| Scenario | Monthly Cost (API) | Monthly Cost (Self-Hosted) | Savings |
|----------|-------------------|---------------------------|---------|
| 100M tokens/day, 8B model | $1,500 (GPT-4o-mini) | $200 (1× A10G) | 7.5× |
| 100M tokens/day, 70B model | $3,000 (GPT-4o) | $800 (2× A100) | 3.75× |
| 1B tokens/day, 8B model | $15,000 (GPT-4o-mini) | $600 (3× A10G) | 25× |
| 1B tokens/day, 70B model | $30,000 (GPT-4o) | $2,400 (6× A100) | 12.5× |

The cost advantage grows with scale.

---

## 10. Implementation Phases

### Phase 1: Minimum Viable LLM (4–6 weeks)

**Goal**: Serve a single open-source model on a single node with basic billing.

| Task | Crate | Effort |
|------|-------|--------|
| Add `wasmtime-wasi-nn` dependency | runtime | 2 days |
| Register WASI-NN in linker | runtime | 1 day |
| Write LLM gateway WASM module | apps/ | 3 days |
| Integrate llama.cpp as WASI-NN backend | runtime | 5 days |
| Add `[llm]` config section | common, node | 2 days |
| Model download + SHA-256 verification | node | 2 days |
| SSE streaming in Pingora | proxy | 3 days |
| `LlmBillingRecord` + token counting | billing, common | 3 days |
| `wasm-ctl model deploy` | ctl | 2 days |
| Basic E2E test (deploy model, infer, bill) | e2e | 3 days |

**Deliverable**: `curl https://llm.my-platform.com/v1/chat/completions` returns a
streaming response from a self-hosted Llama 3.1 8B model, with token-based billing.

### Phase 2: Multi-Tenant & LoRA (3–4 weeks)

| Task | Crate | Effort |
|------|-------|--------|
| Per-tenant WASM gateway modules | supervisor | 3 days |
| LoRA adapter deployment via artifact server | node, ctl | 3 days |
| LoRA hot-loading in native backend | runtime | 3 days |
| Token quota enforcement | billing | 2 days |
| KV cache externalization to redb | storage, runtime | 5 days |
| Per-model routing (route to node with model) | proxy | 3 days |
| LLM policy profile (Step 33 extension) | common | 2 days |

**Deliverable**: Multiple tenants each get their own LoRA-adapted model, with isolated
billing and token quotas.

### Phase 3: Production Hardening (4–6 weeks)

| Task | Crate | Effort |
|------|-------|--------|
| GPU health monitoring | node, metrics | 3 days |
| Request queuing with backpressure | proxy | 3 days |
| Multi-node model distribution | messaging, node | 5 days |
| Chaos tests (GPU OOM, model corruption) | e2e | 3 days |
| Structured logging for LLM inference | all | 2 days |
| Token-based billing reports | billing | 3 days |
| Admin API auth for LLM endpoints | proxy | 2 days |
| Documentation + runbooks | docs | 3 days |

**Deliverable**: Production-ready self-hosted LLM platform with monitoring, billing,
and multi-node scaling.

---

## 11. Risks & Mitigations

### Risk 1: WASI-NN Backend Compatibility

**Risk**: `wasmtime-wasi-nn` may not support the latest llama.cpp features (paged
attention, LoRA, speculative decoding).

**Mitigation**: The WASI-NN API is a standard interface. If the official crate lags,
we can implement custom host functions that call llama.cpp directly. This bypasses
WASI-NN but keeps the same sandbox boundary.

### Risk 2: GPU Memory Fragmentation

**Risk**: Loading/unloading LoRA adapters repeatedly may fragment GPU memory, causing
OOM even when total free VRAM appears sufficient.

**Mitigation**: Use llama.cpp's built-in memory management (mmap-based, not malloc).
Limit the number of concurrent LoRA adapters per model. Implement a LoRA adapter LRU
cache with configurable max count.

### Risk 3: Cold KV Cache Latency

**Risk**: Moving KV cache pages from redb to RAM adds latency (disk read + deserialize).

**Mitigation**: Only cold pages are in redb. Hot sessions stay in RAM. The redb read
path is <1ms for typical page sizes. This is acceptable for long-context sessions where
the alternative is re-computing the entire context (seconds of GPU time).

### Risk 4: Multi-Tenant Security on Shared GPU

**Risk**: A bug in the native backend could leak KV cache data between tenants.

**Mitigation**: This is the same risk accepted by every shared-GPU cloud provider.
Mitigations: separate processes per backend, zero-on-free memory policy, regular
security audits of the native backend.

### Risk 5: Model License Compliance

**Risk**: Some open-source models have usage restrictions (e.g., Llama's acceptable
use policy). Self-hosting does not eliminate license obligations.

**Mitigation**: The platform should store license metadata alongside model files and
enforce usage restrictions via the WASM gateway module.

---

## 12. Verdict

### It Is Completely Possible

The Wasm Cloud Platform **can** serve self-hosted open-source LLMs. The architecture is
sound, the changes are bounded, and the economics strongly justify it.

### The Architecture Is Sound

The hybrid WASI-NN delegation model works:
- **WASM gateway modules** (~100KB) handle auth, billing, quotas, token streaming
- **llama.cpp** (native) handles inference, quantization, CPU offloading, GPU, KV cache
- **WASI-NN** is the bridge between them
- **redb** serves as cold KV cache tier (local, not remote)
- **Pingora** routes and rate-limits
- **NATS** coordinates model deployment and health

### The "Memory Problem" Is Solved

Quantization + CPU offloading + GQA models means:
- Llama 3.1 8B Q4 fits on **any modern GPU** (4.5 GB)
- Llama 3.1 70B Q4 fits on **one A100** or **two consumer GPUs** or **CPU + RAM**
- Only 400B+ models need distributed inference (rarely necessary with LoRA)

### The Changes Are Bounded

The adaptation requires:
- **1 major change**: WASI-NN integration in `crates/runtime`
- **4 moderate changes**: Supervisor lifecycle, proxy SSE, billing tokens, node GPU detection
- **4 minor changes**: Common types, messaging events, CLI commands, config sections
- **0 architectural rewrites**: The shared-nothing, fuel-metering, NATS control plane,
  Pingora proxy, and redb storage all remain as-is

### What You Get That vLLM/Ollama Do Not Provide

1. **Multi-tenant isolation**: Each tenant's LLM gateway runs in a WASM sandbox
2. **Per-tenant billing**: Hash-chain token accounting, not just GPU-hour metering
3. **LoRA as a service**: Deploy/remove adapters without restarting the model
4. **Unified ops**: Same `wasm-ctl`, same NATS, same Prometheus, same redb for LLM and HTTP apps
5. **Cost control**: Token quotas per tenant, rate limiting, request queuing
6. **Zero API dependency**: You own the entire stack end-to-end

### What You Give Up

1. **Simplicity**: More moving parts than `ollama serve`
2. **Raw throughput**: WASI-NN call overhead adds ~0.1ms per inference call (negligible
   for LLM, but measurable for very high QPS)
3. **Cutting-edge features**: New llama.cpp features may lag behind WASI-NN support
4. **GPU cluster scheduling**: The platform manages one GPU per node, not a GPU cluster

### Final Recommendation

**Proceed with Phase 1.** The minimum viable LLM (single model, single node, basic
billing) can be built in 4–6 weeks and immediately provides cost savings. The platform's
architecture is well-suited for this extension — the changes are additive, not disruptive.
The hybrid WASI-NN model is the correct approach, and the existing operational
infrastructure (proxy, billing, NATS, storage) provides real value that standalone
inference servers lack.

The biggest risk is not technical — it is **GPU availability and cost**. If you don't
already have GPU hardware, cloud GPU pricing ($0.50–1.50/hr) narrows the savings
margin. But if you own hardware, or if your token volume justifies dedicated GPUs,
self-hosting on this platform is **dramatically cheaper** than any API provider.

---

## Completion Checklist

### Feasibility Assessment
- [x] Memory constraints analyzed (4GB WASM limit vs model sizes)
- [x] Quantization + CPU offloading identified as primary solution
- [x] GQA models identified as KV cache reduction strategy
- [x] Cold start conflict resolved (warm pool model)
- [x] Fuel metering gap resolved (token-based billing)
- [x] GPU access path identified (WASI-NN + llama.cpp native backend)
- [x] KV cache architecture analyzed (local hot, redb cold, remote for DR only)
- [x] Layer streaming from STUDIES evaluated (not needed — quantization + offloading solves it)
- [x] llama.cpp WASM build evaluated (demo only, not production)
- [x] Cost analysis completed with break-even points

### Architecture Design
- [x] Hybrid WASI-NN delegation model defined
- [x] WASM gateway module responsibilities specified
- [x] Native backend responsibilities specified (llama.cpp handles all inference)
- [x] KV cache externalization to redb designed (local cold tier)
- [x] Remote KV cache analyzed (DR only, not real-time)
- [x] LoRA adapter lifecycle defined
- [x] Configuration schema defined (`[llm]` section)

### Component Impact Analysis
- [x] `crates/runtime` — WASI-NN integration (major)
- [x] `crates/supervisor` — LLM instance lifecycle (major)
- [x] `crates/storage` — Model metadata + KV cache (moderate)
- [x] `crates/proxy` — SSE streaming (moderate)
- [x] `crates/billing` — Token-based accounting (moderate)
- [x] `crates/common` — LLM types (minor)
- [x] `crates/messaging` — Model events (minor)
- [x] `crates/node` — GPU detection (moderate)
- [x] `crates/ctl` — LLM CLI commands (minor)
- [x] Components that do NOT need changes listed

### Dependency on INFRA_IMPL 31–37
- [x] Critical prerequisites identified (31, 32, 33, 34, 37)
- [x] Non-blocking dependencies identified (35, 36)
- [x] New design documents needed (39, 40, 41, 42, 43)

### Risk Assessment
- [x] WASI-NN compatibility risk
- [x] GPU memory fragmentation risk
- [x] Cold KV cache latency risk
- [x] Multi-tenant security risk
- [x] License compliance risk

### Implementation Roadmap
- [x] Phase 1: Minimum Viable LLM (4–6 weeks)
- [x] Phase 2: Multi-Tenant & LoRA (3–4 weeks)
- [x] Phase 3: Production Hardening (4–6 weeks)

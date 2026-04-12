# Wasm Cloud Platform — Implementation Master Plan

This directory contains the full technical implementation plan for the Wasm-native cloud
infrastructure. Each file corresponds to one buildable layer. Implement them in order —
each layer depends on the previous.

---

## Why This Platform Exists

### The Problem

Modern cloud platforms (Kubernetes, ECS, Fly.io) are built around **containers**. Containers
are a great abstraction — but they carry significant weight:

- **Cold start**: A container starts in 1–10 seconds (image pull, OS init, runtime boot)
- **Per-app overhead**: Each container has its own kernel namespace, network stack, and process
- **Isolation boundary**: Process isolation is good, but the kernel is shared — a kernel exploit
  can escape any container
- **Operational complexity**: A full container stack requires a scheduler (K8s), a container
  registry, a CNI plugin, an ingress controller, and a service mesh for proper isolation
- **Binary portability**: A container image is architecture-specific; `amd64` images cannot run
  on `arm64` without emulation

**This platform replaces containers with WebAssembly modules.** The benefits:

| Problem                 | Container                        | Wasm module                                       |
| ----------------------- | -------------------------------- | ------------------------------------------------- |
| Cold start              | 1–10 seconds                     | < 10ms                                            |
| Memory overhead per app | 50–200 MB (OS + runtime)         | 1–5 MB (code + linear memory)                     |
| Isolation               | Linux namespaces (kernel shared) | Hardware-enforced SFI (no kernel escape)          |
| Binary portability      | Architecture-specific            | Architecture-neutral (compile once, run anywhere) |
| Multi-tenancy           | 1 pod per tenant                 | 1000s of Wasm modules on a single node            |
| Deployment complexity   | K8s + registry + CNI + ingress   | Single binary, one NATS cluster                   |

This is not a replacement for containers in every case. It is a purpose-built platform for
**stateless, HTTP-serving applications** where startup latency and density matter.

---

## Core Design Philosophy

Every architectural decision flows from these five principles:

### 1. Shared-Nothing
Each node is fully self-sufficient. `redb` (an embedded database) holds binaries, configs,
and secrets locally on disk. There is no central database that nodes depend on.

**Why**: A central DB is a single point of failure and a network hop on every cold start.
With shared-nothing, a node with a full disk failure loses only the apps on that node —
the rest of the cluster is unaffected.

### 2. Fuel Metering (Deterministic CPU Accounting)
CPU is not measured in milliseconds. It is measured in **Fuel** — a counter that decrements
by 1 on every Wasm instruction. Fuel is deterministic: the same program with the same inputs
always consumes the same number of fuel units, regardless of system load.

**Why**: Wall-clock time is a poor proxy for CPU usage in a multi-tenant system. A program
that does 10ms of CPU and a program that sleeps for 10ms look the same to a clock, but they
have radically different impacts on other tenants. Fuel measures actual computation.

### 3. Cold Start < 10ms
Binaries are compiled from `.wasm` bytecode to native machine code **exactly once** using
AOT (Ahead-of-Time) compilation with Cranelift. The compiled artifact is serialized and
stored in `redb`. On cold start, the Supervisor loads the artifact from disk and
deserializes it — no recompilation, just a memory map. This takes < 1ms for most apps.

**Why**: If cold start is < 10ms, you can run in a truly serverless mode — kill idle
instances, spin them up on demand, and the user never notices.

### 4. East-West vs North-South Traffic Separation
- **North-South** (external): HTTP/HTTPS from the internet enters via Pingora (the proxy).
  Pingora handles TLS termination, rate limiting, and routing.
- **East-West** (internal): App A calling App B on the same node goes through the Supervisor
  directly, never leaving the host. This avoids a round-trip through the network stack.

**Why**: East-West traffic through a proxy doubles the latency. Direct routing inside the
Supervisor keeps intra-node calls sub-millisecond.

### 5. NATS as the Control Plane
All cross-node coordination (deploy, secret rotation, service discovery) flows through NATS.
NATS is a lightweight, high-throughput message bus. Nodes subscribe to subjects; events
are published once and fan out to all subscribers simultaneously.

**Why**: A REST-based control API requires knowing which nodes are alive and polling them.
NATS pub/sub inverts this — nodes receive commands reactively, and the operator does not
need to enumerate or address individual nodes.

---

## Full System Lifecycle

### Lifecycle 1: Deploying an Application

```
Operator runs: wasm-ctl deploy --app api-users --binary ./api-users.wasm
       │
       ▼
wasm-ctl uploads .wasm binary to Node-0's artifact HTTP server (port 9091)
       │
       ▼
wasm-ctl publishes Event::DeployApp { artifact_url, sha256, config } to NATS
       │
       ├─── Node-0 receives event:
       │      1. Fetches binary from local artifact store (already uploaded)
       │      2. Verifies SHA-256 hash
       │      3. Compiles .wasm → native artifact via Cranelift (CPU-intensive, spawn_blocking)
       │      4. Stores artifact bytes in redb [artifacts table]
       │      5. Stores AppConfig in redb [configs table]
       │      6. App is now IDLE: code on disk, no instance running
       │
       ├─── Node-1 receives same event via NATS:
       │      1. Fetches binary from Node-0's artifact server (HTTP GET)
       │      2. Same compile + store flow
       │      3. App is IDLE on Node-1 too
       │
       └─── Node-2: same as Node-1
```

The deployment is **push-based**: the operator runs one command, all nodes in the cluster
receive and process the binary independently. No central coordinator is needed.

### Lifecycle 2: Serving an HTTP Request (Cold Start)

```
User sends: GET https://api.myapp.com/users
       │
       ▼
Pingora (proxy on port 443) receives the request
       │  Looks up Host header "api.myapp.com" in HostRouter table
       │  Finds: AppId = "api-users:v2"
       │
       ▼
Pingora checks UpstreamRegistry for "api-users:v2"
       │  Result: no instances running (app is IDLE)
       │
       ▼
Pingora calls cold_start(app_id) on the Supervisor
       │
       ▼
Supervisor::ensure_instance("api-users:v2")
       │  1. Loads compiled artifact from redb (< 1ms)
       │  2. Allocates host port 10347 from PortAllocator
       │  3. Resolves env vars: static config + decrypted secrets
       │  4. Calls spawn_blocking → Wasmtime deserializes artifact, creates WASI Preview 2 Component env
       │  5. Wasm module starts its internal Tokio runtime, binds to port 10347
       │  6. Supervisor probes TCP port 10347 until it accepts connections
       │  7. Registers port 10347 in UpstreamRegistry for "api-users:v2"
       │  Total time: < 10ms
       │
       ▼
Pingora retries the upstream lookup → finds port 10347
Pingora forwards GET /users to http://127.0.0.1:10347/users
       │
       ▼
Wasm Axum app handles the request, sends HTTP 200 back
       │
       ▼
Pingora returns response to user
```

### Lifecycle 3: Scaling (Warm Path)

Once an instance is running, subsequent requests are fast-path:
```
Request arrives → Pingora → UpstreamRegistry returns 127.0.0.1:10347 immediately
→ Request forwarded without waking Supervisor
```

When load grows:
```
Supervisor health loop detects instance is saturated (fuel consumption high)
→ spawn() called for additional instance → new port allocated
→ UpstreamRegistry now has two addresses for the same app
→ Pingora round-robins across both
```

### Lifecycle 4: Hot-Swap Deploy (Zero Downtime)

```
Operator deploys api-users:v3 (new version)
       │
       ▼
All nodes compile and store the v3 artifact (IDLE state)
       │
       ▼
Deploy protocol begins:
  1. Supervisor spawns v3 instance → registers in UpstreamRegistry
  2. Pingora starts routing some requests to v3
  3. v2 instance is removed from UpstreamRegistry (no new requests)
  4. v2 waits for in-flight requests to drain (graceful shutdown)
  5. v2 instance stops, port released
       │
       ▼
100% traffic on v3, zero requests dropped
```

---

## Architecture Diagram (Extended)

```
┌──────────────────────────────────────────────────────────────────────┐
│                           NODE BINARY (wasm-node)                    │
│                                                                      │
│  ┌─────────────────────┐     ┌──────────────────────────────────┐   │
│  │  PINGORA PROXY      │     │        SUPERVISOR                │   │
│  │  (North-South only) │     │  (Instance lifecycle manager)    │   │
│  │                     │     │                                  │   │
│  │  - TLS termination  │     │  - spawn / health / prune        │   │
│  │  - Host → AppId     │◄────│  - cold start callback           │   │
│  │  - Round-robin LB   │     │  - fuel-based autoscale          │   │
│  │  - Rate limiting    │     │  - port allocator (10000-19999)  │   │
│  │  - Health checks    │────►│  - config validation at deploy   │   │
│  └──────────┬──────────┘     └────────────┬─────────────────────┘   │
│             │                             │                          │
│             │ (UpstreamRegistry)          │ (WasmRuntime)            │
│             │                             │                          │
│             │         ┌───────────────────▼──────────────────────┐  │
│             │         │           WASMTIME ENGINE                 │  │
│             │         │  (Cranelift AOT — shared per process)     │  │
│             │         │                                           │  │
│             │         │  PreparedModule             Wasm Instance │  │
│             │         │  (deserialized artifact) → (running app)  │  │
│             │         │                                           │  │
│             │         │  - Fuel metering (per-request quota)      │  │
│             │         │  - Memory limits via Tunables             │  │
│             │         │  - I/O limits (fd, fs, net egress)        │  │
│             │         │  - WASI sockets (pre-bound by Supervisor) │  │
│             │         └──────────────────────────────────────────┘  │
│             │                                                        │
│  ┌──────────▼──────────────────────────────────────────────────┐    │
│  │                         REDB (local disk)                    │    │
│  │                                                              │    │
│  │  [artifacts]     compiled Wasm blobs (AOT, native)           │    │
│  │  [configs]       AppConfig per app (fuel, memory, env vars)  │    │
│  │  [secrets]       DEK-encrypted secret bundles per app        │    │
│  │  [metrics]       1-minute aggregated execution buckets       │    │
│  │  [routes]        Host → AppId routing table                  │    │
│  │  [raw_wasm]      original .wasm bytes (for re-compilation)   │    │
│  │  [billing]       hash-chained per-request fuel records        │    │
│  │  [_schema_meta]  schema version for migration tracking       │    │
│  └──────────────────────────┬───────────────────────────────────┘   │
│                             │                                        │
│  ┌──────────────────────────▼───────────────────────────────────┐   │
│  │                  NATS BUS (async-nats)                        │   │
│  │                                                               │   │
│  │  Pub subjects:  instance.ready, instance.dead, node.load      │   │
│  │  Sub subjects:  deploy.app.new, secrets.update.>, config.>    │   │
│  │  JetStream:     DEPLOY stream (durable, replayed on join)     │   │
│  └──────────────────────────────────────────────────────────────┘   │
│                             │                                        │
└─────────────────────────────┼────────────────────────────────────────┘
                              │
                    NATS ◄────┼────► OTHER NODES (same subjects)
                              │
                   Operator ──┘ (wasm-ctl CLI via NATS + admin HTTP)
```

---

## Technology Choices & Rationale

### Why Wasm/WASI (not containers)?
Containers start in seconds, use hundreds of MB per instance, and share the kernel.
Wasm modules start in milliseconds, use single-digit MB, and are hardware-isolated via
Software Fault Isolation (SFI). For high-density multi-tenant HTTP serving, Wasm wins.

### Why Wasmtime (not Wasmer)?
Both are production-grade. Wasmtime natively supports the modern WASI Preview 2 (Component Model),
which offers much better native capability-based network security. We switched from Wasmer/WASIX
(a legacy extension) because Wasmtime's Component Model aligns strictly with standards, enforcing
network access cleanly via mechanisms like `socket_addr_check`.

### Why Cranelift (not LLVM)?
LLVM produces faster code but takes 5–30 seconds to compile a typical Axum app. Cranelift
produces code within 5% of native speed and compiles in < 1 second. Since we compile once
on deploy and run the artifact on every cold start, compilation time matters enormously.

### Why redb (not SQLite, RocksDB, or PostgreSQL)?
- Pure Rust (no C FFI, no linking issues)
- MVCC: reads never block writes (critical — metrics writes and artifact reads are concurrent)
- Typed tables (generics): no string-based queries, no schema confusion
- Zero-config: one file, no daemon

See [02_STORAGE_REDB.md](02_STORAGE_REDB.md) for the full comparison table.

### Why Pingora (not NGINX, Envoy, or Axum)?
- Written in Rust: same language as the node, one binary
- Thread-per-core model with zero-copy: extremely low latency
- Dynamic upstream table: we can add/remove instances without reloading config
- Used in production at Cloudflare for >400M requests/second

### Why NATS (not Kafka, Redis, or etcd)?
- Sub-millisecond publish/subscribe latency
- No broker state on the publisher side: fire-and-forget
- JetStream adds durable, at-least-once delivery for deploy events
- A single NATS cluster handles the entire control plane (no separate service registry)

---

## Key Challenges The System Resolves

| Challenge                         | Solution                                                                                                              |
| --------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| **Multi-tenancy isolation**       | Wasm SFI: no shared memory, no kernel escape possible                                                                 |
| **Cold start latency**            | AOT compilation + artifact caching: < 10ms cold start                                                                 |
| **CPU fairness**                  | Fuel metering: every tenant gets a deterministic computation quota                                                    |
| **I/O resource abuse**            | Extended limits: per-instance fd caps, fs write limits, network egress caps, connection limits                        |
| **Secret security at rest**       | DEK/KEK hierarchy: per-app encryption keys, master key never on disk in plaintext                                     |
| **Config validation**             | Deploy-time validation: missing secrets, reserved name conflicts, and field sanity checks rejected before first spawn |
| **Zero-downtime deploys**         | Blue-green drain: v3 starts before v2 stops                                                                           |
| **Automatic rollback**            | Trap-rate and health-failure thresholds trigger automatic rollback to the previous version                            |
| **Node failure resilience**       | Shared-nothing: redb on each node, NATS JetStream for replay                                                          |
| **Disaster recovery**             | L1–L6 failure classification: integrity checks, partition handling, multi-node recovery playbook                      |
| **DDoS / abuse protection**       | Token bucket rate limiting at the proxy layer — per-tenant and per-IP, with backpressure signaling                    |
| **DB connection explosion**       | pgBouncer sidecar: 1000 Wasm instances → 20 real DB connections                                                       |
| **Log visibility**                | WASI stdout/stderr capture + structured log forwarding                                                                |
| **NATS control-plane health**     | Consumer lag, stream disk usage, connection state, and redelivery monitoring with Prometheus alerts                   |
| **Cluster state sync**            | JetStream replay for apps/routes; direct NATS request/reply for secrets                                               |
| **Wasm binary size**              | Artifact server: NATS has 1MB limit; binaries are served via HTTP                                                     |
| **Disk growth / artifact sprawl** | Version-based artifact GC: retain 3 versions, prune metrics older than 7 days, undeploy cleanup                       |
| **Schema evolution**              | redb schema version table + idempotent migration runner                                                               |
| **Network isolation**             | Pre-bound sockets: Wasm app cannot bind arbitrary ports                                                               |
| **Platform upgrades**             | Rolling binary upgrades with protocol versioning; MessageEnvelope wire compatibility                                  |
| **Per-tenant billing**            | Tamper-evident fuel accounting: hash-chained per-request records, S3 export for invoicing                             |
| **DNS / TLS integration**         | Wildcard and custom domain support, route-change webhooks, ACME certificate automation                                |

---

## Implementation Order

| #   | File                              | What it builds                                                       | Depends on     |
| --- | --------------------------------- | -------------------------------------------------------------------- | -------------- |
| 01  | `01_WORKSPACE_SETUP.md`           | Cargo workspace, crates layout, all dependencies                     | —              |
| 02  | `02_STORAGE_REDB.md`              | Local persistent store (artifacts, config, metrics, secrets)         | 01             |
| 03  | `03_WASM_RUNTIME.md`              | Wasmtime integration — AOT compile, fuel metering, memory limits     | 02             |
| 04  | `04_WASI_NETWORKING.md`           | WASI Preview 2 components — virtual sockets, port allocation/binding | 03             |
| 05  | `05_ENV_CONFIG.md`                | Environment variable injection into WasiEnv                          | 03, 02         |
| 06  | `06_SECRETS.md`                   | Encrypted secret store, DEK/KEK hierarchy, NATS rotation             | 02             |
| 07  | `07_SUPERVISOR_CORE.md`           | Supervisor lifecycle — spawn, health, prune, scaling loop            | 03, 04, 05, 06 |
| 08  | `08_NATS_MESSAGING.md`            | NATS bus — deploy events, secret updates, service discovery          | 07             |
| 09  | `09_PROXY_PINGORA.md`             | Pingora proxy — dynamic upstream table, TLS, health checks           | 07, 08         |
| 10  | `10_DEPLOYMENT_PROTOCOL.md`       | Hot-swap deploy, graceful shutdown, zero-downtime rollout            | 07, 08, 09     |
| 11  | `11_METRICS_OBSERVABILITY.md`     | Prometheus metrics, OpenTelemetry tracing, Grafana                   | 07             |
| 12  | `12_SCALING.md`                   | Fuel-based auto-scaling, request steering, concurrency               | 07, 09         |
| 13  | `13_SECURITY.md`                  | Multi-tenancy isolation, sandboxing, network policies                | All            |
| 14  | `14_NODE_ENTRYPOINT.md`           | `main.rs` — wires all crates, CLI args, systemd unit                 | All            |
| 15  | `15_ROUTE_MANAGEMENT.md`          | Host→AppId routing table, redb persistence, NATS sync                | 02, 08, 09     |
| 16  | `16_ARTIFACT_REGISTRY.md`         | HTTP artifact server, large binary distribution (>1MB)               | 02, 08         |
| 17  | `17_WASM_LOGS.md`                 | stdout/stderr capture, structured log forwarding, SSE tail           | 07             |
| 18  | `18_ADMIN_CLI.md`                 | `wasm-ctl` operator CLI — deploy, routes, secrets, logs              | 08             |
| 19  | `19_CLUSTER_BOOTSTRAP.md`         | New node state sync, join handshake, snapshot transfer               | 08, 15, 16     |
| 20  | `20_GRACEFUL_SHUTDOWN.md`         | Wasm instance drain — TCP close, HTTP endpoint, SIGTERM              | 07, 09         |
| 21  | `21_DATABASE_CONNECTIONS.md`      | pgBouncer sidecar, connection pool limits                            | 05             |
| 22  | `22_STORAGE_SCHEMA_VERSIONING.md` | redb schema versions, automatic migration, backup                    | 02             |
| 23  | `23_INTEGRATION_TESTING.md`       | Unit, E2E, chaos, and load test strategy                             | All            |
| 24  | `24_RATE_LIMITING.md`             | Token bucket rate limiting, per-tenant + per-IP, backpressure        | 09, 07         |
| 25  | `25_PLATFORM_UPGRADES.md`         | Rolling binary upgrades, protocol versioning, wire compatibility     | 08, 14         |
| 26  | `26_ARTIFACT_GC.md`               | Artifact + metrics garbage collection, disk pressure monitoring      | 02, 07, 11     |
| 27  | `27_DISASTER_RECOVERY.md`         | Failure classification (L1–L6), integrity checks, partition handling | 02, 08, 07     |
| 28  | `28_BILLING_ACCOUNTING.md`        | Per-request fuel accounting, hash-chain tamper evidence, S3 export   | 07, 11, 02     |
| 29  | `29_DNS_INTEGRATION.md`           | DNS prerequisites, wildcard/custom domains, TLS, health endpoints    | 09, 15         |

---

## Core Concepts Summary

- **Shared-Nothing**: Every node is self-sufficient. `redb` holds binaries, configs, and secrets locally. No central DB.
- **Fuel Metering**: CPU is measured in deterministic units (Fuel), not wall-clock time. Enables fair multi-tenancy.
- **Cold Start < 10ms**: Binaries are AOT-compiled once and cached in `redb`. Instantiation = memory load, no compilation.
- **East-West Traffic**: App-to-App on the same node goes through the Supervisor directly, never Pingora.
- **North-South Traffic**: Only external HTTP enters via Pingora (TLS termination, rate limiting).

---

## Key Crates Reference

| Purpose              | Crate                                                |
| -------------------- | ---------------------------------------------------- |
| Wasm runtime         | `wasmtime`                                           |
| WASI support         | `wasmtime-wasi`                                      |
| Proxy                | `pingora`, `pingora-proxy`, `pingora-load-balancing` |
| Async runtime        | `tokio` (full features)                              |
| Local DB             | `redb`                                               |
| Messaging            | `async-nats`                                         |
| Encryption           | `aes-gcm`, `chacha20poly1305`                        |
| Metrics              | `prometheus`, `opentelemetry`                        |
| Serialization        | `serde`, `serde_json`, `bincode`                     |
| Hashing (billing)    | `sha2`                                               |
| HTTP client (health) | `reqwest`                                            |
| Logging              | `tracing`, `tracing-subscriber`                      |
| CLI (admin)          | `clap`                                               |

# wasm-ctl

CLI tool for managing the Wasm Cloud Platform.

## Overview

`wasm-ctl` is the command-line interface for interacting with the Wasm Cloud Platform. It communicates with running nodes via NATS events and the HTTP admin API, providing a convenient interface for operators to deploy, manage, and inspect Wasm applications across the cluster.

## Architecture

The CLI follows a standard command-and-handler pattern:

1. **Command Parsing** — Clap-based CLI with subcommands mapped to `*Cmd` structs.
2. **Client Layer** — Builds HTTP clients for the Admin API and NATS connections for event-based commands.
3. **Handler Execution** — Each command handler interacts with the platform through the appropriate transport (HTTP for synchronous operations, NATS for event-driven operations).
4. **Output Formatting** — Results are formatted and printed to stdout/stderr.

For a private NATS PKI, configure `--nats-ca-cert`,
`--nats-client-cert`, and `--nats-client-key` (or the corresponding
`WASM_CTL_NATS_*` environment variables). The client certificate and key must
be supplied together. `--nats-creds` remains available for NATS user/account
credentials and can be combined with mutual TLS.

### Commands

| Command | Description |
|---------|-------------|
| `deploy` | Deploy a Wasm application to the cluster |
| `remove` | Remove a deployed application |
| `list` | List deployed applications |
| `instances` | Show running instances for an app |
| `routes` | Display HTTP routing table |
| `secrets` | Manage application secrets |
| `app` | Application-level operations |
| `logs` | Stream application logs |
| `logging` | Configure logging levels |
| `status` | Platform status overview |
| `platform` | Platform information |
| `gc` | Trigger garbage collection |
| `node` | Node management operations |
| `cluster` | Cluster-wide operations |
| `billing` | Billing and usage information |
| `policy` | Policy management |
| `gateway` | Gateway configuration |

### Remote deploys

`wasm-ctl deploy` supports three artifact sources:

- local file via `--wasm`
- remote URL via `--artifact-url` plus `--sha256`
- OCI reference via `--artifact-ref`

Remote artifact deploys use the deploy ingress endpoint. Configure it with:

- `--deploy-api`
- or `WASM_CTL_DEPLOY_API`

This can be separate from `--node-api` / `WASM_CTL_NODE_API`, which remain relevant for local artifact upload and other node/admin operations.

Examples:

```bash
wasm-ctl deploy --app hello --version v1 --wasm ./hello.wasm

wasm-ctl deploy \
  --app hello \
  --version v1 \
  --artifact-url https://artifacts.example.com/hello.wasm \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef

wasm-ctl deploy \
  --app hello \
  --version v1 \
  --artifact-ref oci://ghcr.io/example-org/hello:v1 \
  --artifact-credential ghcr-reader
```

Manifest-driven deploys can now publish public route bindings as part of the
deploy itself. Both of these shapes are supported:

```toml
[gateway]
host = "www.example.com"

[[gateway.routes]]
host = "api.example.com"
path_prefix = "/v1"
strip_prefix = false
```

`gateway.host` is kept as the compatibility shorthand for the default `/`
route. `[[gateway.routes]]` supports additional host and path bindings for the
same app.

If deploy ingress is running with `WASM_DEPLOY_INGRESS_REQUIRE_OCI_DIGEST_REFS=true`,
mutable OCI tag refs like `oci://...:v1` are rejected and callers must use
digest-pinned refs.

Optional signed-artifact metadata can be attached on remote deploys:

```bash
wasm-ctl deploy \
  --app hello \
  --version v1 \
  --artifact-ref oci://ghcr.io/example-org/hello@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --artifact-public-key <base64-ed25519-public-key> \
  --artifact-signature <base64-ed25519-signature> \
  --artifact-signature-algorithm ed25519 \
  --artifact-issuer https://token.actions.githubusercontent.com \
  --artifact-repository example-org/hello \
  --artifact-namespace production
```

For Cosign-style signed payload verification with a public key:

```bash
wasm-ctl deploy \
  --app hello \
  --version v1 \
  --artifact-ref oci://ghcr.io/example-org/hello@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --artifact-public-key <base64-ed25519-public-key> \
  --artifact-signature <base64-ed25519-signature> \
  --artifact-signature-algorithm cosign-ed25519 \
  --artifact-signature-payload '{"critical":{"identity":{"docker-reference":"ghcr.io/example-org/hello"},"image":{"docker-manifest-digest":"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"},"type":"cosign container image signature"},"optional":{"issuer":"https://token.actions.githubusercontent.com","repository":"example-org/hello","namespace":"production"}}'
```

`cosign-ed25519` verifies a Cosign-style signed payload with the supplied public
key and then applies the normal deploy-ingress issuer/repository/namespace
policy checks. It does not implement Fulcio/Rekor verification by itself.

For Sigstore bundle verification:

```bash
wasm-ctl deploy \
  --app hello \
  --version v1 \
  --artifact-url https://artifacts.example.com/hello.wasm \
  --sha256 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef \
  --artifact-public-key <base64-ed25519-public-key> \
  --artifact-signature <base64-ed25519-signature> \
  --artifact-signature-algorithm sigstore-bundle \
  --artifact-signature-payload "$(cat artifact.sigstore.json)" \
  --artifact-identity user@example.com \
  --artifact-issuer https://github.com/login/oauth
```

This mode verifies a Sigstore bundle against Sigstore’s trust root. The current
policy binding for it is issuer + identity.

Artifact fetch credentials are managed separately from runtime app secrets:

```bash
export WASM_CTL_DEPLOY_API=https://deploy.example.com
wasm-ctl secrets set-artifact-credential --key ghcr-reader
```

Those credentials are stored in the deploy-ingress credential store under `_platform/artifact-credentials:v1` and are used only for deploy-time artifact fetch.

For OCI refs, the node resolves tags or digests during deploy ingress, fetches the registry manifest if needed, then downloads the final blob and verifies its content hash before publishing the normal deploy event.

## Public API

This crate produces the `wasm-ctl` binary. It is not intended to be used as a library.

### Key Types

- `*Cmd` structs — Parsed representations of each CLI subcommand
- `build_http_client()` — Constructs the HTTP client for Admin API communication
- `update_config()` — Fetches and updates CLI configuration from the platform

## Known Issues & Improvements

### Reliability

- **`build_http_client` panics on failure** — Should return a `Result` instead of calling `panic!`, making the CLI resilient to misconfiguration.
- **Errors printed but not returned** — Several functions print errors to stderr and return `Ok(())`, swallowing failures and making programmatic error detection impossible.
- **`update_config` silently falls back to defaults on fetch failure** — Network or server errors are hidden; the user has no indication that their config is stale.

### Functionality Gaps

- **`SecretsCmd::Delete` not implemented** — The delete subcommand is a stub; secrets cannot be removed through the CLI.
- **SSE parsing is naive** — The Server-Sent Events parser does not handle multi-line events or partial chunks, leading to dropped or malformed log output.

### Concurrency & Performance

- **Blocking I/O in async context** — `Store::open` and `std::fs::write` are called inside async functions, blocking the Tokio runtime.
- **Entire WASM file read into memory before uploading** — Large modules can cause excessive memory usage; streaming upload would be more efficient.

### Usability

- **Default value comparison for `--version` is fragile** — Explicitly passing `--version v1` is treated as the default, making it impossible to specify the default version explicitly.
- **Auth token silently skipped if contains invalid header characters** — No error or warning is emitted; the request proceeds unauthenticated.

### Testing

- **No tests for most commands** — The majority of CLI commands lack automated test coverage.

## Security Considerations

- **Secret rotation now uses per-node ciphertext, not plaintext fanout** — `wasm-ctl secrets set` fetches the authoritative cluster node registry and encrypts one `SecretUpdate` event per active node using that node's advertised X25519 transport public key. Operators should still protect NATS access because metadata such as app IDs, secret keys, and target node IDs remain visible.
- **Auth token silently skipped on invalid characters** — If a token contains characters invalid for HTTP headers, the token is dropped without warning. This can cause requests to be sent without authentication, potentially exposing admin endpoints.
- **No certificate pinning or TLS verification options** — The HTTP client does not expose options for custom CA certificates or certificate pinning, making it difficult to secure admin API communication in restricted environments.
```

<file_path>
Wasm-Cloud-Platform\crates\e2e\README.md
</file_path>

<edit_description>
Create README.md for the e2e crate
</edit_description>

---

```
# e2e

End-to-end and chaos testing framework for the Wasm Cloud Platform.

## Overview

The `e2e` crate provides a comprehensive testing framework for validating the Wasm Cloud Platform under both normal and adverse conditions. It includes cluster fixtures that spin up NATS containers and `wasm-node` processes, fault injection across OSI layers L1–L6, recovery verification with Time-To-Recovery (TTR) measurement, structured test reports, and pre-built chaos scenarios.

## Architecture

### Test Infrastructure

```
ClusterFixture
├── NatsContainer (podman-based NATS server)
├── NodeProcess[] (wasm-node instances)
└── Verifier (HTTP/NATS health checks)
```

1. **ClusterFixture** — Sets up a complete test cluster with NATS and one or more node processes.
2. **Fault Injection** — `inject_*` methods apply network partitions, disk latency, memory pressure, and other faults at specified OSI layers.
3. **Recovery Verification** — After fault removal, the verifier measures TTR and checks system invariants.
4. **Test Reporting** — Results are collected into structured `TestReport` objects for analysis.

### Fault Injection Layers

| Layer | Fault | Method |
|-------|-------|--------|
| L1 | Physical / link | `inject_nats_partition` |
| L2 | Data link | — |
| L3 | Network | `inject_nats_partition` (tc qdisc) |
| L4 | Transport | Port-level partitioning |
| L5 | Session | — |
| L6 | Presentation | `inject_disk_latency`, `inject_memory_pressure` |

## Public API

### Key Types

- **`ClusterFixture`** — Sets up and tears down test clusters with NATS and node processes.
- **`NodeProcess`** — Represents a managed `wasm-node` process with lifecycle control.
- **`InjectionResult`** — Result of a fault injection operation.
- **`TestReport`** — Aggregated test results with pass/fail/skip counts and timing.
- **`TestResult`** — Individual test outcome.
- **`CheckResult`** — Result of a single verification check.
- **`VerificationResult`** — Outcome of a recovery verification including TTR.

### Feature Flags

- `chaos-linux` — Linux-specific chaos tests (defined but unused in code).
- `chaos-basic` — Basic chaos scenarios (defined but unused in code).
- `chaos-full` — Full chaos test suite (defined but unused in code).

## Known Issues & Improvements

### Reliability

- **Port allocation can collide** — The formula `process_id * 2 % 30000` can produce collisions when multiple test processes run concurrently.
- **`inject_nats_partition` uses `tc qdisc` on loopback** — This affects ALL loopback traffic, not just NATS, potentially disrupting other tests or services.
- **`inject_disk_latency` doesn't actually inject latency** — The implementation writes a 100MB temp file instead of applying I/O scheduling delays.
- **`inject_memory_pressure` only waits 100ms** — The memory allocation may not complete in time, making the injection unreliable.

### Code Quality

- **`tests/harness.rs` duplicates ~760 lines from `src/fixture.rs` and `src/helpers.rs`** — This massive duplication increases maintenance burden and risk of divergence.
- **`NatsContainer` uses the host container runtime directly** — the test harness shells out to Podman or Docker instead of going through an extra library layer.
- **Several E2E tests NOT marked `#[ignore]`** — Tests that require NATS will fail in environments without it.

### Performance

- **New `reqwest::Client` created per request** — The verifier and helpers create a new HTTP client for every request, incurring unnecessary TLS handshake and connection overhead.

### Safety

- **`register_cleanup` replaces previous ctrl-c handler** — If multiple fixtures register cleanup handlers, only the last one runs, potentially leaving resources behind.
- **`ensure_hosts_entry` modifies `/etc/hosts`** — If cleanup fails, the host's `/etc/hosts` file is left in a modified state.

## Security Considerations

- **Hardcoded PostgreSQL password in tests** — Database credentials are embedded in source code. While this is a test crate, it normalizes insecure credential handling and could leak if the repository is shared.
- **`ensure_hosts_entry` modifies system files** — Writing to `/etc/hosts` requires elevated privileges and can break host networking if cleanup fails. Consider using namespace isolation or DNS overrides instead.
- **No isolation between test clusters** — Concurrent test runs share the same network namespace and may interfere with each other's fault injection (especially `tc qdisc` rules).
```

<file_path>
Wasm-Cloud-Platform\crates\ebpf-monitor\README.md
</file_path>

<edit_description>
Create README.md for the ebpf-monitor crate
</edit_description>

---

```
# ebpf-monitor

Kernel-level observability using eBPF with userspace fallback.

## Overview

`ebpf-monitor` provides deep system observability for the Wasm Cloud Platform by leveraging eBPF programs on Linux (kernel >= 5.8) or falling back to userspace polling on other platforms. It monitors process lifecycle, TCP connections, file descriptors, memory pressure, disk I/O latency, and syscall anomalies. Detected events are dispatched through `ActionDispatcher`, which updates Prometheus metrics and triggers recovery actions via the `EventCallbacks` trait.

## Architecture

```
┌─────────────────────────────────────┐
│           eBPF Programs             │
│  (process, tcp, fd, memory, disk,   │
│   syscall tracepoints/kprobes)      │
└──────────────┬──────────────────────┘
               │ perf buffer
               ▼
┌─────────────────────────────────────┐
│         ActionDispatcher            │
│  ┌─────────────┐ ┌───────────────┐  │
│  │  Prometheus  │ │ EventCallbacks│  │
│  │  Metrics     │ │ (recovery)    │  │
│  └─────────────┘ └───────────────┘  │
└─────────────────────────────────────┘
               │
               ▼ (if eBPF unavailable)
┌─────────────────────────────────────┐
│       Fallback Monitor              │
│  (5-second poll interval)           │
└─────────────────────────────────────┘
```

### Monitoring Domains

| Domain | eBPF Attachment | Metrics |
|--------|----------------|---------|
| Process lifecycle | sched tracepoints | Process spawn/exit counts |
| TCP connections | sock tracepoints | Connection count, state |
| File descriptors | fd install tracepoint | FD count per process |
| Memory pressure | OOM kprobe | OOM events |
| Disk I/O latency | block tracepoints | Latency histograms |
| Syscall anomalies | raw_syscalls tracepoint | Syscall frequency counts |

## Public API

### Key Types

- **`MonitorHandle`** — Handle to the running monitor, used to query status and update configuration.
- **`MonitorStatus`** — Current status of the monitor (running, stopped, error).
- **`MonitorConfig`** — Configuration including thresholds and poll intervals.
- **`EbpfMetrics`** — Snapshot of all eBPF-collected metrics.
- **`ActionDispatcher`** — Dispatches monitor events to metrics and callback handlers.
- **`EventCallbacks`** — Trait for handling recovery actions; implement to customize response to events.
- **`NoopCallbacks`** — Default no-op implementation of `EventCallbacks`.
- **`MonitorEvent`** — Enum of all observable event types.
- **`RecoveryAction`** — Actions to take in response to detected anomalies.
- **`LoadedEbpf`** — Wrapper around loaded eBPF object with type-safe accessors.

## Known Issues & Improvements

### Reliability

- **`update_thresholds` / `current_config` use `unwrap()` on `RwLock`** — If a thread panics while holding the lock, subsequent accesses will panic due to lock poisoning. Use `lock().unwrap_or_else(|e| e.into_inner())` or handle the poison case.
- **`PENDING_IO_COUNT` can become inconsistent** — Missed completion events leave stale entries, inflating the in-flight I/O count.
- **`block_rq_requeue` handler commented out** — Requeued block requests leave stale entries in `IO_START_TIME`, causing latency miscalculation.
- **`out_of_memory` kprobe commented out** — OOM events are not detected when this is disabled.

### Correctness

- **TCP/FD count tracking includes ALL processes** — The monitor tracks system-wide counts rather than filtering to `wasm-node` children, making metrics noisy and thresholds unreliable.
- **`dev_minor` extraction incorrect for extended `dev_t` format** — On modern kernels with large device numbers, the minor number is extracted incorrectly, causing disk I/O events to be attributed to wrong devices.
- **`parse_event` uses `unsafe read_unaligned` without data validation** — Malformed or truncated perf buffer data can cause undefined behavior.

### Deployment

- **Hardcoded eBPF object paths** — Object paths point to development directories and won't work in production deployments.
- **`include_bytes_aligned!` production path not implemented** — The production code path for embedding eBPF objects is missing.

### Performance

- **`syscall_counter` monitors ALL syscalls on system** — This has significant performance impact on busy systems; it should be filtered to target processes.
- **Fallback monitor hardcodes 5-second poll interval** — Not configurable; may be too frequent or too infrequent depending on workload.
- **No rate limiting on `dispatch` method** — A burst of kernel events can overwhelm the dispatcher and callback handlers.

### Code Quality

- **`register_metric!` macro creates orphaned metrics on registration failure** — Failed registrations leave partial metric state that cannot be cleaned up.
- **Per-endpoint rate limiting not implemented** — Rate limiting is mentioned in design but not implemented.

## Security Considerations

- **eBPF requires `CAP_BPF` / `CAP_SYS_ADMIN`** — The monitor needs elevated privileges to load eBPF programs. In containerized deployments, this must be carefully scoped.
- **`syscall_counter` traces all syscalls system-wide** — This is a broad monitoring scope that could expose sensitive system activity from unrelated processes.
- **No validation of perf buffer data** — Corrupted or malicious perf buffer contents could cause undefined behavior in `parse_event` due to `unsafe read_unaligned`. Input validation should be added.
- **Hardcoded paths in development builds** — If development builds are accidentally deployed, the eBPF object paths could leak information about the build environment.
```

<file_path>
Wasm-Cloud-Platform\crates\internal_gateway\README.md
</file_path>

<edit_description>
Create README.md for the internal_gateway crate
</edit_description>

---

```
# internal_gateway

Transparent internal gateway for East-West traffic between applications.

## Overview

`internal_gateway` provides service-to-service communication within the Wasm Cloud Platform. It listens on loopback port 9080 and acts as a transparent proxy, parsing the `Host` header in `<app>.<namespace>.internal` format to resolve the target application, apply policies, and forward the request. This enables zero-configuration service discovery for inter-app communication.

## Architecture

```
App A ──HTTP──▶ :9080 ──parse_internal_host──▶ resolve app
                      │                         │
                      │                    apply policies
                      │                         │
                      │                    forward request
                      ▼                         ▼
                  InternalGateway ──HTTP──▶ App B (target)
```

1. **Request Ingestion** — Listens on `127.0.0.1:9080` for internal HTTP traffic.
2. **Host Parsing** — Extracts app name and namespace from the `Host` header using `parse_internal_host()`.
3. **Policy Application** — Checks authentication, authorization, and rate limits.
4. **Request Forwarding** — Proxies the request to the resolved target application.

## Public API

### Key Types

- **`InternalGateway`** — Main gateway struct that binds to the loopback port, parses incoming requests, and forwards them to target applications.
- **`parse_internal_host()`** — Parses a `Host` header value in `<app>.<namespace>.internal` format, returning the app name and namespace.

### Configuration

- **Listen address** — `127.0.0.1:9080` (loopback only)
- **Host format** — `<app>.<namespace>.internal`

## Known Issues & Improvements

### Correctness

- **`parse_internal_host` fails for app names containing dots** — Since dots are used as delimiters, an app named `my.service` in namespace `prod` would be parsed incorrectly from `my.service.prod.internal`.
- **`target_app_id` constructed without version** — The target application lookup may fail if the configuration requires a versioned app ID.

### Reliability

- **Request body read with `usize::MAX` limit** — This creates an out-of-memory risk; a malicious or misconfigured client could send an extremely large body.
- **No request timeout on forwarding client** — Requests to unresponsive upstream applications will hang indefinitely, consuming resources.

### Performance

- **New `reqwest::Client` created per request** — This is extremely wasteful; each client creation involves TLS configuration, connection pool setup, and DNS resolution. A shared client should be used.
- **Full body buffering before forwarding** — The gateway reads the entire request body into memory before forwarding, adding latency and memory pressure. Streaming the body would be more efficient.

### Functionality Gaps

- **No namespace isolation enforcement** — Any app can reach any namespace; there is no boundary enforcement between namespaces.
- **`EndpointAuth::Authenticated` and `Roles` are no-op placeholders** — Authentication and authorization checks are defined but not implemented; all requests are effectively unauthenticated.
- **Per-endpoint rate limiting not implemented** — Rate limiting is referenced in the policy types but not enforced.

### Code Quality

- **`ConnectInfo` `peer_addr` captured but unused** — The client address is extracted from the connection but never used for logging, rate limiting, or access control.

## Security Considerations

- **No namespace isolation** — Apps in one namespace can reach apps in any other namespace. This violates the principle of least privilege and could allow lateral movement in a compromise scenario.
- **Authentication and authorization are no-ops** — `EndpointAuth::Authenticated` and `Roles` are placeholder types with no enforcement. Any request reaching the gateway is forwarded without identity verification.
- **No request timeout** — Without a timeout, slowloris-style attacks can consume gateway resources by holding connections open indefinitely.
- **Unbounded request body** — Reading the request body with `usize::MAX` limit allows memory exhaustion attacks. A reasonable limit should be enforced.
- **No rate limiting** — Without per-endpoint rate limiting, a misbehaving or compromised app can flood downstream services.
- **Loopback-only binding mitigates external access** — The gateway only listens on `127.0.0.1`, which prevents direct external access. However, any local process or Wasm app can reach it.
```

<file_path>
Wasm-Cloud-Platform\crates\node\README.md
</file_path>

<edit_description>
Create README.md for the node crate
</edit_description>

---

```
# wasm-node

Main binary crate for the Wasm Cloud Platform node.

## Overview

`wasm-node` is the core runtime of the Wasm Cloud Platform. It connects to NATS for cluster communication, deploys and manages Wasm applications, routes HTTP traffic via an embedded Pingora proxy, provides an Admin API built with Axum, runs an eBPF monitor for observability, handles cluster bootstrap and leader election, orchestrates rolling upgrades, manages database connectivity (PostgreSQL), supports hot-reloadable configuration, and coordinates graceful shutdown.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                    wasm-node                         │
│                                                     │
│  ┌──────────┐  ┌──────────┐  ┌───────────────────┐ │
│  │  NATS    │  │  Admin   │  │  Pingora Proxy    │ │
│  │  Client  │  │  API     │  │  (HTTP routing)   │ │
│  └────┬─────┘  └────┬─────┘  └────────┬──────────┘ │
│       │             │                 │             │
│  ┌────▼─────────────▼─────────────────▼──────────┐  │
│  │              App Supervisor                    │  │
│  │  (Wasm instance lifecycle, scaling, routing)   │  │
│  └───────────────────┬───────────────────────────┘  │
│                      │                              │
│  ┌───────────┐  ┌────▼─────┐  ┌──────────────────┐ │
│  │  eBPF     │  │ Database │  │  Config          │ │
│  │  Monitor  │  │ (PG)     │  │  (hot-reload)    │ │
│  └───────────┘  └──────────┘  └──────────────────┘ │
│                                                     │
│  ┌──────────────────────────────────────────────┐   │
│  │  Cluster: Bootstrap, Leader Election,        │   │
│  │  Rolling Upgrades, State Snapshots           │   │
│  └──────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────┘
```

### Subsystems

| Subsystem | Responsibility |
|-----------|---------------|
| NATS Client | Cluster communication, event streaming, subscriptions |
| Admin API | Management endpoints (Axum), authentication, token rotation |
| Pingora Proxy | HTTP traffic routing to Wasm application instances |
| App Supervisor | Wasm instance lifecycle, scaling, health checks |
| eBPF Monitor | Kernel-level observability and anomaly detection |
| Database | PostgreSQL connectivity for persistent state |
| Config | Hot-reloadable configuration from files/NATS |
| Cluster | Bootstrap, leader election, rolling upgrades, state snapshots |

## Public API

This crate produces the `wasm-node` binary. It is not intended to be used as a library.

### Key Entry Points

- **`main()`** — Single monolithic entry point (~2100 lines) handling all subsystem initialization and event loops.

## Known Issues & Improvements

### Concurrency & Deadlocks

- **`block_on` inside async runtime in eBPF callbacks** — Calling `block_on` from within a Tokio runtime context can deadlock or panic. eBPF callbacks should schedule work on the runtime instead.
- **`event_tx` channel receiver immediately dropped** — The receiver for the event channel is dropped on creation, meaning all events sent through `event_tx` go nowhere.

### Cluster Coordination

- **Leader election logic inverted** — Multiple nodes respond as leader instead of just the smallest node ID, causing split-brain scenarios.
- **Rolling upgrade cluster node list always returns only `self.node_id`** — The ordering/selection logic is broken, preventing proper upgrade orchestration.
- **`WaitForPredecessor` upgrade action not handled** — The action is logged and dropped, leaving upgrades in an inconsistent state.
- **`bootstrap_keypair` is `None` for existing nodes** — Nodes that rejoin the cluster cannot receive encrypted secrets.

### Shutdown & Lifecycle

- **`begin_graceful_shutdown` is a no-op** — The function just sleeps; it doesn't drain or stop running instances, leaving them running indefinitely.
- **`handle_state_snapshot` uses fixed 100ms sleep for artifact arrival** — This is a race condition; the artifact may not have arrived in 100ms, or the sleep may unnecessarily delay processing.

### NATS & Messaging

- **Duplicate NATS subscriptions with same consumer name** — Multiple subscriptions sharing a consumer name causes unreliable message delivery.
- **Subscription subjects ignored** — The `_subject` parameter is unused; subscriptions receive ALL messages regardless of the intended filter.

### Configuration & Routing

- **`NodeLoad` handler hardcodes supervisor address to `127.0.0.1:9000`** — The supervisor address should be configurable.
- **`SecretUpdate` handler stores encrypted value without decryption** — Secrets are stored in their encrypted form, making them unreadable by applications.
- **DNS stub resolves ALL `*.internal` domains to `127.0.0.1`** — No allowlist is enforced; any internal domain resolves to loopback.

### Code Quality

- **Massive `main()` function (~2100 lines)** — The entry point handles initialization, event loops, and business logic. It should be decomposed into modules.
- **`fetch_artifact` duplicates `upgrade::download_and_verify`** — Shared logic should be extracted into a common function.

## Security Considerations

- **Admin API tokens sent over plaintext HTTP** — Authentication tokens are transmitted without TLS, allowing network-level token theft.
- **Token rotation endpoint returns new token in response body** — The new token is exposed in the HTTP response, which may be logged or cached.
- **`admin/rebuild` endpoint deletes database without confirmation** — A single API call can destroy all persistent state with no safeguard.
- **`admin/gc/force` kills instances for ALL apps** — Including currently deployed applications, causing unintended downtime.
- **DNS stub resolves ALL `*.internal` domains to `127.0.0.1`** — Without an allowlist, this can be exploited for DNS rebinding or to redirect traffic.
- **`SecretUpdate` stores encrypted values without decryption** — This effectively corrupts secrets, making them unusable and potentially causing application failures that mask the real issue.
- **eBPF threshold updates logged but not propagated** — Threshold changes are not applied to running eBPF programs, meaning security-relevant monitoring may be operating with stale thresholds.

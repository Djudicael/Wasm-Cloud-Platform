# WASI Policy Enforcement — Known Limitations

This document tracks the gaps between the Step 33 spec
(`INFRA_IMPL/33_WASI_POLICY_ENFORCEMENT.md`) and the current implementation.
Each limitation is classified by root cause so it is clear what must change
before the gap can be closed.

## Current authoritative boundary

The active runtime policy boundary is a combination model built on top of
Wasmtime, not a plan to replace the engine:

- TCP bind/connect address policy is enforced in `WasiCtxBuilder::socket_addr_check(...)`
- DNS is enforced only as a coarse on/off switch through `allow_ip_name_lookup(...)`
- filesystem visibility is enforced through `preopened_dir(...)`
- memory/table growth is enforced through `ResourceLimiter` and counted through `PolicyEnforcer`
- per-instance namespace and loopback restrictions can be tightened further by the supervisor's extra socket gate
- eBPF remains the outer layer for cross-checking and future finer-grained enforcement where Wasmtime does not expose hooks

This means the authoritative question is capability-specific:

- some capabilities are already authoritatively enforced in-process,
- some are enforced and authoritatively counted through the existing Wasmtime hooks,
- some remain future work because Wasmtime does not expose the required write/stream hooks yet.

## Capability guarantee matrix

This is the operator-facing boundary summary for the current runtime:

| Capability | Primary layer today | Enforced on live path | Authoritative counters | Deferred / notes |
|---|---|---:|---:|---|
| TCP bind | Wasmtime `socket_addr_check` | Yes | Yes | Supervisor may apply stricter per-instance bind rules on top. |
| TCP connect | Wasmtime `socket_addr_check` | Yes | Yes | CIDR filtering and outbound-connection reservation happen before the supervisor extra gate. |
| UDP socket address allow/deny | Wasmtime `socket_addr_check` | Yes | No | Address policy is live; UDP byte / operation accounting is not yet exported authoritatively. |
| DNS lookup enable/disable | Wasmtime network toggle | Yes | No | Only coarse on/off exists today; no hostname-level allowlist and no authoritative DNS counters. |
| Filesystem path visibility | Wasmtime preopens | Yes | No | `allowed_paths` are authoritative for visibility; read/write byte accounting is not. |
| Memory/table growth | Wasmtime `ResourceLimiter` | Yes | Yes | Current usage, peaks, and denied growth requests are exported through `PolicyEnforcer`. |
| Filesystem write-byte limits | Outer layer / future host wrapping | No | No | Writable host paths are opt-in only; byte-accurate enforcement is still future work. |
| TCP egress-byte limits | Outer layer / future host wrapping | No | No | The counter exists in policy code, but no per-write Wasmtime hook drives it today. |

Practical reading:

- If a policy relies on `tcp_bind`, `tcp_connect`, filesystem preopen visibility, or memory/table caps, the runtime itself is the primary enforcement boundary today.
- If a policy relies on byte-accurate filesystem writes, byte-accurate TCP egress, or fine DNS controls, the runtime does not yet provide authoritative in-process enforcement. Those cases still rely on outer layers, operational constraints, or future host/resource wrapping on top of Wasmtime.

## Root Cause Classification

| Tag | Meaning |
|---|---|
| **library** | The upstream dependency (Wasmtime) does not expose the required API. Cannot be fixed without a Wasmtime change or major workaround. |
| **complexity** | The required API exists; the work is integration code that has not been written yet. Fixable within the project. |
| **design** | A design decision is needed before implementation can proceed. |

---

## 1. Per-Connection CIDR Filtering

**Status:** Enforced in the runtime socket hook
**Root cause:** `complexity` closed, broader host-wrapping still `design`
**Spec reference:** §4 — `check_tcp_connect_policy()` should filter by destination IP
**Code location:** `crates/runtime/src/executor.rs` — `WasiCtxBuilder` setup

### What the spec says

When a Wasm module calls `socket.connect("169.254.169.254:80")`, the WASI host
should check the destination IP against `allowed_cidrs` / `denied_cidrs` and
return `EACCES` if the IP is not permitted.

### What actually happens

The runtime uses:

```rust
builder.socket_addr_check(...)
```

and routes TCP connect decisions through `PolicyEnforcer::check_outbound_tcp_connect()`.
That means destination-IP / CIDR filtering and outbound connection-count
reservation happen on the live runtime path before the optional supervisor gate.

What still does **not** exist is deeper per-resource host wrapping for every
network capability. The current solution is built on Wasmtime's socket callback
hook, not on wrapped `TcpSocket` resources.

### Defense in depth

1. **WASI runtime socket hook** — enforces per-connect allow/deny and counters.
2. **Supervisor extra socket gate** — can apply namespace / local-service rules on top.
3. **eBPF layer** — remains available for kernel-level cross-checking and future enforcement.

### Fix path

- **Current state:** closed for TCP connect CIDR enforcement on the runtime socket path.
- **Remaining future work:** if the project wants every network operation to go
  through deeper custom policy-aware host/resource wrappers instead of only
  Wasmtime's existing callback hooks, that is a larger design/integration task.

---

## 2. PolicyTcpStream Wrapper (Per-Write Egress Enforcement)

**Status:** Not implemented
**Root cause:** `library` — WASI Preview 2 Component Model
**Spec reference:** §4 — `PolicyTcpStream` with `write_with_policy()`
**Code location:** `crates/runtime/src/executor.rs` — after `WasiCtxBuilder` setup

### What the spec says

A `PolicyTcpStream` wrapper should intercept every `write()` on a TCP socket,
count the bytes, and return an error if `max_egress_bytes` is exceeded.

### What actually happens

In WASI Preview 2's Component Model, sockets are opaque resource handles
managed inside a `ResourceTable`. The host creates the handle, but once the
Wasm module holds it, all `write()` calls go through Wasmtime's internal WASI
implementation — the host cannot intercept individual writes.

The `PolicyEnforcer::check_egress()` and `record_egress()` methods exist but
must be called explicitly from host code. There is no automatic per-write hook.

### Fix path

- **Short term:** Egress byte counting is available when the host code calls
  `record_egress()` manually (e.g., from a proxy layer). The WASI layer
  provides the counter, not the enforcement.
- **Medium term:** If Wasmtime adds a writable-stream wrapper or write-hook
  API, `PolicyTcpStream` can be implemented as designed in the spec.
- **Alternative:** Implement egress enforcement at the eBPF layer by counting
  bytes sent per PID and killing the process when the limit is exceeded.

---

## 3. Preopened Directories from `allowed_paths`

**Status:** Implemented
**Root cause:** closed
**Spec reference:** §3 — `WasiCtxBuilder` configured from `InstancePolicy`
**Code location:** `crates/runtime/src/executor.rs` — after `builder.env("PORT", ...)`

### What the spec says

Each path in `policy.filesystem.allowed_paths` should be preopened via
`WasiCtxBuilder::preopened_dir()`, giving the Wasm module access only to those
directories. Paths not in the list should be invisible to the module.

### What actually happens

The runtime now maps `policy.filesystem.allowed_paths` into
`WasiCtxBuilder::preopened_dir(...)` calls and derives read-only vs read-write
permissions from the existing filesystem policy flags.

### Fix path

Closed in the current runtime. The remaining filesystem gap is not path
visibility; it is the lack of authoritative per-open/per-write host-call
accounting for all guest file activity.

The operational default is now stricter than before:

- no host filesystem writes are permitted unless the app config grants at least
  one explicit absolute `allowed_path`
- a positive write budget without such a writable path is rejected during policy
  resolution instead of being silently accepted as misleading no-op config

---

## 4. eBPF Coordination and Workload Identity

**Status:** Implemented for the supported single-trust-domain model
**Root cause:** closed for TID lifecycle; process-per-application isolation remains `design`
**Spec reference:** §7 — runtime execution identity registered in eBPF maps
**Code location:** `crates/supervisor/src/spawn_runtime.rs` and `shutdown_runtime.rs`

### What the spec says

When a Wasm instance starts, its observable execution identity must be
registered with the eBPF monitor and removed on every stop/failure path.
System-wide probes must not observe unrelated host workloads.

### What actually happens

WASI applications execute in the `wasm-node` process on dedicated
single-thread Tokio runtimes. The supervisor registers each runtime TID and its
namespace/application identity in every `MONITORED_TIDS` map before execution,
and deregisters it on normal completion, cancellation, failure, and shutdown.
TCP-close events use the port-to-TID correlation table to release persistent
outbound-connection reservations.

Block-I/O and memory-pressure probes are scoped to the dedicated `wasm-node`
cgroup-v2 ID. This prevents observation of unrelated host cgroups but does not
turn applications inside one process into mutually isolated tenants. Buffered
writeback may execute in kernel-worker context and is not claimed as exact
per-application block-I/O attribution.

### Fix path

- Production must set `runtime.isolation_mode = "single-trust-domain"` and
  `ebpf.enabled = true`, `ebpf.required = true`; admission rejects weaker or
  misleading production settings.
- The operator must run each node in a dedicated cgroup-v2 cgroup.
- Mutually untrusted tenants require a future process-per-application mode,
  separate cgroups/credentials, and new cross-tenant non-observation and escape
  testing. The current platform must not be advertised for that deployment.
- Runtime `PolicyCounters` and Prometheus policy metrics remain distinct
  accounting layers; kernel observations do not make unsupported byte-accurate
  WASI quotas authoritative.

---

## 5. Automatic Policy Violation → Prometheus Counter Pipeline

**Status:** Partially implemented
**Root cause:** `complexity`
**Spec reference:** §6 — `PolicyMetrics` with Prometheus counters
**Code location:** `crates/metrics/src/exporter.rs` — `PolicyMetrics` struct

### What the spec says

Every time a policy check denies an operation, the corresponding Prometheus
counter should be incremented automatically.

### What actually happens

`PolicyMetrics` is defined with all 6 counters and 2 gauges, and is registered
with the Prometheus registry. However, the `PolicyEnforcer` increments its own
`PolicyCounters` (atomic counters inside the runtime) but does **not** also
increment the `PolicyMetrics` Prometheus counters.

The two counter systems are not connected:

- `PolicyCounters` — per-instance, atomic, lives in `StoreState`
- `PolicyMetrics` — global, Prometheus-backed, lives in `Metrics`

### Fix path

Add a reference to `PolicyMetrics` in the `PolicyEnforcer` (or pass it as a
callback) so that when a denial is recorded, the global Prometheus counter is
also incremented. For example:

```rust
pub struct PolicyEnforcer {
    pub policy: InstancePolicy,
    pub counters: Arc<PolicyCounters>,
    pub metrics: Option<Arc<PolicyMetrics>>,  // add this
}
```

When `check_outbound_tcp_connect()` returns a denial, also call:
```rust
if let Some(ref m) = self.metrics {
    m.connection_denied_total.inc();
}
```

The gauges (`active_outbound_connections`, `open_fds`) need periodic scraping
from all live instances — this can be done in the supervisor's health tick.

**Estimated effort:** Small–Medium (2–4 hours).

---

## 6. DNS Exfiltration Prevention

**Status:** Coarse only
**Root cause:** `library` (same as #1)
**Spec reference:** §13 — Security Considerations
**Code location:** `crates/runtime/src/executor.rs` — `allow_ip_name_lookup()`

### What the spec says

DNS should be either fully allowed or fully denied. A sophisticated attacker
could use DNS queries to exfiltrate data even when TCP/UDP are blocked.

### What actually happens

`allow_ip_name_lookup(bool)` is the only control available. There is no way to
allow DNS but restrict which hostnames can be resolved, or to limit the size
of DNS responses.

### Fix path

- **Short term:** The `allow_dns` policy flag provides a clean on/off switch.
  Apps that don't need DNS should set `allow_dns: false` (e.g., `StaticSite`
  profile already does this).
- **Long term:** A custom DNS resolver could be injected that filters queries
  by domain allowlist. This would require Wasmtime to support a custom
  `IpNameLookup` provider, which it currently does not.

---

## Summary Table

| # | Limitation | Root Cause | Fixable Without Wasmtime Changes? | Estimated Effort |
|---|---|---|---|---|
| 1 | Per-connection CIDR filtering | library | No | Requires Wasmtime hook API |
| 2 | PolicyTcpStream (per-write egress) | library | No | Requires Wasmtime stream wrapper API |
| 3 | Preopened directories from `allowed_paths` | complexity | **Yes** | Small (1–2h) |
| 4 | eBPF PID registration + counter export | complexity | **Yes** | Medium (4–8h) |
| 5 | PolicyCounters → Prometheus pipeline | complexity | **Yes** | Small–Medium (2–4h) |
| 6 | DNS exfiltration (hostname filtering) | library | No | Requires custom DNS resolver API |

### Two-Layer Defense (Current Architecture)

Even with limitations #1 and #2, the system provides meaningful security through
defense in depth:

```
┌─────────────────────────────────────────────────────┐
│ Layer 3: WASI Host (this step)                      │
│   ✅ Coarse protocol on/off (TCP, UDP, DNS)         │
│   ✅ Per-instance connection counting                │
│   ✅ Per-instance FD limits                          │
│   ✅ Per-instance filesystem write limits            │
│   ✅ CIDR audit logging (denials recorded)           │
│   ❌ Per-connection CIDR enforcement                 │
│   ❌ Per-write egress byte enforcement               │
├─────────────────────────────────────────────────────┤
│ Layer 8: eBPF Monitor (Step 30)                     │
│   ✅ Per-syscall observation at kernel level         │
│   ✅ Per-PID connection tracking                     │
│   ✅ Process killing on violation                    │
│   ⚠️  Reactive (kills after violation, not before)  │
│   ⚠️  Not yet wired to PolicyEnforcer (gap #4)      │
└─────────────────────────────────────────────────────┘
```

The WASI layer **prevents** most violations before they happen (coarse but
proactive). The eBPF layer **detects** anything that slips through (granular
but reactive). Together they provide the security posture described in the
Step 13 defense-in-depth model.

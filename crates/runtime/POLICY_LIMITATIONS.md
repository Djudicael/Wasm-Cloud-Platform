# WASI Policy Enforcement — Known Limitations

This document tracks the gaps between the Step 33 spec
(`INFRA_IMPL/33_WASI_POLICY_ENFORCEMENT.md`) and the current implementation.
Each limitation is classified by root cause so it is clear what must change
before the gap can be closed.

## Root Cause Classification

| Tag | Meaning |
|---|---|
| **library** | The upstream dependency (Wasmtime) does not expose the required API. Cannot be fixed without a Wasmtime change or major workaround. |
| **complexity** | The required API exists; the work is integration code that has not been written yet. Fixable within the project. |
| **design** | A design decision is needed before implementation can proceed. |

---

## 1. Per-Connection CIDR Filtering

**Status:** Not enforced at the WASI layer
**Root cause:** `library` — Wasmtime 43.x
**Spec reference:** §4 — `check_tcp_connect_policy()` should filter by destination IP
**Code location:** `crates/runtime/src/executor.rs` — `WasiCtxBuilder` setup

### What the spec says

When a Wasm module calls `socket.connect("169.254.169.254:80")`, the WASI host
should check the destination IP against `allowed_cidrs` / `denied_cidrs` and
return `EACCES` if the IP is not permitted.

### What actually happens

`WasiCtxBuilder` only exposes coarse on/off switches:

```rust
builder.allow_tcp(true);          // all TCP allowed or none
builder.allow_udp(false);         // all UDP blocked or none
builder.allow_ip_name_lookup(true); // DNS on or off
```

There is **no hook** to intercept an individual `connect()` call, inspect the
destination IP, and reject it before the kernel socket is created. The
`PolicyEnforcer::check_outbound_tcp_connect()` method exists and performs the
CIDR check correctly, but nothing in the Wasmtime runtime calls it
automatically — it can only be called manually from host code.

### Defense in depth

The two-layer defense still works:

1. **WASI layer** — coarse `allow_tcp(bool)` blocks all TCP if the policy says
   no outbound TCP at all.
2. **eBPF layer** (Step 30) — can inspect individual `connect()` syscalls at the
   kernel level and kill the process if the destination is not in `allowed_cidrs`.

### Fix path

- **Short term:** Document that CIDR enforcement relies on eBPF (Layer 8) for
  per-connection granularity. The WASI layer provides the coarse switch.
- **Medium term:** Monitor Wasmtime releases for socket-level interception
  hooks. The Component Model's resource-based architecture may eventually
  allow host-side wrappers around `TcpSocket` resources.
- **Alternative:** Replace `inherit_network()` with a custom network provider
  that wraps each socket in a policy-checking layer. This is a large
  undertaking and would need to track Wasmtime's internal WASI implementation.

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

**Status:** Not wired up
**Root cause:** `complexity`
**Spec reference:** §3 — `WasiCtxBuilder` configured from `InstancePolicy`
**Code location:** `crates/runtime/src/executor.rs` — after `builder.env("PORT", ...)`

### What the spec says

Each path in `policy.filesystem.allowed_paths` should be preopened via
`WasiCtxBuilder::preopened_dir()`, giving the Wasm module access only to those
directories. Paths not in the list should be invisible to the module.

### What actually happens

The builder calls `inherit_stdout()` and `inherit_stderr()` but never calls
`preopened_dir()`. The Wasm module currently inherits the host's full
filesystem with no restrictions at the WASI layer.

### Fix path

This is straightforward integration work:

```rust
// Pseudocode for the fix
for path in &policy.filesystem.allowed_paths {
    let dir = std::fs::File::open(path)?;
    let permissions = if policy.filesystem.allow_file_create {
        wasmtime_wasi::DirPerms::all()
    } else {
        wasmtime_wasi::DirPerms::READ
    };
    builder.preopened_dir(dir, path, permissions, wasmtime_wasi::FilePerms::all())?;
}
```

The API exists in `wasmtime-wasi`. The work is mapping `allowed_paths` to
`preopened_dir()` calls with the correct permissions derived from
`allow_file_create` and `allow_file_delete`.

**Estimated effort:** Small (1–2 hours of coding + testing).

---

## 4. eBPF Coordination (PID Registration + Counter Export)

**Status:** Not wired up
**Root cause:** `complexity`
**Spec reference:** §7 — Instance PIDs registered in eBPF `MONITORED_PIDS` map
**Code location:** `crates/supervisor/src/lib.rs` — after instance ready event

### What the spec says

When a Wasm instance starts, its host PID should be registered in the eBPF
monitor's `MONITORED_PIDS` BPF map so the kernel-level monitor can enforce
per-process limits. The `PolicyCounters` should also be exported to the eBPF
metrics pipeline for cross-referencing.

### What actually happens

Both subsystems exist independently:

- The eBPF monitor (Step 30) has a `MONITORED_PIDS` map and can kill processes.
- The `PolicyEnforcer` has atomic `PolicyCounters` that track violations.

But they are not connected. The supervisor spawns instances via
`tokio::task::spawn_blocking()` and never registers the resulting PID with the
eBPF monitor.

### Fix path

1. After `spawn_blocking` starts, retrieve the thread's PID:
   ```rust
   let pid = std::process::id(); // inside spawn_blocking
   ```
2. Send the PID to the eBPF monitor via the existing `SupervisorCommand` channel
   or a new dedicated channel.
3. The eBPF monitor inserts the PID into `MONITORED_PIDS`.
4. On instance shutdown, send a deregistration message.

For counter export, the `PolicyCounters` are already `Arc`-shared and atomic.
The eBPF metrics pipeline can read them periodically or on-demand.

**Estimated effort:** Medium (4–8 hours of integration work + testing).

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

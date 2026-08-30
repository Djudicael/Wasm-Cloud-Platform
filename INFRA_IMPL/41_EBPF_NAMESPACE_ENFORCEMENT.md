# Step 41 — eBPF Namespace Enforcement for Internal Mesh Gateway

## Current implementation status (2026-08-30)

The source-port lookup is synchronous once published, but publication is not:
`inet_sock_set_state` writes a `TidConnection` event to a ring buffer and the
userspace consumer then calls `bind_port()`. The gateway therefore waits up to
50 ms in 1-ms intervals for the binding and returns 401 if it never appears.
It does not fall back to `NamespaceRegistry.port_to_app`, caller headers, or an
anonymous identity. Inactive eBPF and an unavailable map also fail closed.

The currently implemented enforcement is the userspace internal-gateway
authorization decision backed by kernel-observed identity. The SK_MSG/sockmap
design described later in this document remains proposed defense-in-depth and
must not be reported as implemented. Likewise, non-Linux/old-kernel fallback is
not an authorization mode for required namespace enforcement.

The final 2026-08-30 three-node microVM run used literal `.internal` DNS,
resolved exact same-namespace and cross-namespace callers, passed 24 OIDC
realm/client-role and namespace checks, and completed 96/96 concurrent calls
per node. TCP-close events release each mapped instance's outbound connection
reservation; deployments that enforce that limit must require eBPF. Dependency
removal returned 502 on every node without a remote fallback and redeployment
restored 200. Cross-host mesh identity is out of scope by design. See
`INFRA_IMPL/process/INTERNAL_MESH_OIDC_ROLE_VALIDATION.md`.

## Goal

Add **identity attribution** to the internal gateway so it knows *which Wasm
instance* is making each East-West request and *which namespace* that instance
belongs to. This closes the current gap where the gateway port (9080) is open
to all namespaces with no caller identity.

The design uses a **pull-at-decision model**: when the gateway receives a
request, it asks the identity map "who is calling from source port X?" and gets
back the namespace and app ID. The source-port binding reaches that map through
an asynchronous eBPF event and bounded userspace consumer handoff.

Three cooperating layers:

1. **Supervisor TID registration** — When spawning an instance, the Supervisor
   registers its OS Thread ID (TID) with `{namespace, app_id}` in a shared map.
   It also tracks which source ports belong to which TIDs.
2. **Gateway identity query** — On each request, the gateway looks up the
   source port → TID → {namespace, app_id} chain in the same process, after the
   bounded publication handoff described above.
3. **eBPF attribution + audit** — eBPF observes gateway TCP state, publishes
   source-port/TID bindings, and audits forged identity headers. The gateway is
   the current enforcement point. SK_MSG packet dropping remains proposed
   defense-in-depth.

No separate processes. No containers. Everything runs inside the single
`wasm-node` process or is observed by eBPF at the kernel level.

---

## The Problem

### Current State

The Supervisor already has namespace-aware networking:

- `socket_addr_check` blocks cross-namespace connections to **direct app ports**
- `NetworkInterceptor` checks `source_app.namespace() != target_app.namespace()`
- Service discovery only injects `SERVICE_URL` env vars for same-namespace apps

**But the gateway port (9080) is a shared resource with no identity:**

```
App A (ns=production) ──connect──► gateway:9080 ──forward──► App C (ns=production)  ✅
App B (ns=staging)     ──connect──► gateway:9080 ──forward──► App C (ns=production)  ❌ (should deny)
```

The gateway currently has **no idea** who is calling. It parses the `Host`
header to find the target but cannot verify the caller's namespace.

### The Three Gaps

| Gap | Current Behavior | Problem |
|-----|-------------------|---------|
| **No caller identity** | Gateway has no idea who sent the request | Cannot enforce cross-namespace deny-by-default |
| **Port attribution is TOCTOU** | `NamespaceRegistry.port_to_app` maps bind ports, not outbound source ports | Ephemeral source ports are reused; mapping is stale |
| **Headers are unauthenticated** | App could forge `X-Namespace: production` | Gateway has no way to verify |

### Why Not Just Use the WASI Layer?

The `socket_addr_check` callback fires at **connect time** and returns a
`bool` (allow/deny). It cannot:

- Inject identity headers into the TCP stream
- Communicate the caller's identity to the gateway
- Inspect or modify the data being written

The WASI layer controls *whether* a connection happens, but not *what*
travels over it. We need a separate mechanism to attribute identity to
connections that the WASI layer already allowed.

---

## Why eBPF for This Platform

eBPF is the right tool because:

1. **All Wasm instances share one PID** — The `wasm-node` process runs every
   instance. PID-based filtering is useless. But each `spawn_blocking` task
   gets its own OS thread with a unique TID. eBPF's
   `bpf_get_current_pid_tgid()` returns the TID, giving us per-instance
   granularity.

2. **The gateway is a kernel socket** — When a Wasm instance connects to
   port 9080, the TCP handshake happens in the kernel. eBPF observes this
   in real time via the `inet_sock_set_state` tracepoint (already used by
   `tcp_monitor.rs`).

3. **No host code changes needed for audit** — eBPF monitors syscalls from
   outside the process. It doesn't require modifying wasmtime-wasi or the
   Wasm runtime.

4. **Defense in depth** — Even if a Wasm module escapes the WASI sandbox
   (e.g., via a wasmtime vulnerability), it cannot forge its TID. The kernel
   knows which thread made the `send()` call.

---

## Understanding TID in the Wasm Runtime

### How Instances Get Their TIDs

The Supervisor runs each Wasm instance via `tokio::task::spawn_blocking()`:

```rust
// crates/supervisor/src/lib.rs — current code
let task = tokio::task::spawn_blocking(move || {
    let mut instance = prepared_clone.spawn_instance(env_vars, host_port, Some(socket_addr_check))?;
    let _ = spawn_result_tx.send(Ok(()));
    let stats = instance.run(); // blocks this thread until instance exits
    stats
});
```

`spawn_blocking()` allocates a **dedicated OS thread** from Tokio's blocking
pool. The thread is pinned for the entire duration of the closure. Each
instance gets its own TID.

```
wasm-node process (PID = 12345)
├── Thread A (TID = 12346) → AppA (ns=production)
├── Thread B (TID = 12347) → AppB (ns=staging)
└── Thread C (TID = 12348) → AppC (ns=production)
```

### bpf_get_current_pid_tgid()

This eBPF helper returns a `u64`:
- **Upper 32 bits** = TGID = the process PID (same for all threads)
- **Lower 32 bits** = TID (unique per thread)

```rust
// In aya-ebpf Rust:
let pid_tgid: u64 = unsafe { aya_ebpf::helpers::bpf_get_current_pid_tgid() };
let pid: u32 = (pid_tgid >> 32) as u32;   // 12345 (same for all instances)
let tid: u32 = (pid_tgid & 0xFFFFFFFF) as u32; // 12346, 12347, 12348, etc.
```

We use the **lower 32 bits (TID)** as the map key.

### Getting the TID in Userspace

The Supervisor needs the TID from inside the `spawn_blocking` closure:

```rust
#[cfg(target_os = "linux")]
fn gettid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

#[cfg(not(target_os = "linux"))]
fn gettid() -> u32 {
    // Non-Linux: eBPF is not available anyway, so use a pseudo-TID
    // for the fallback map. This won't match eBPF's TID but provides
    // a consistent API for the gateway's in-process identity table.
    std::thread::current().id().as_u64().get() as u32
}
```

### TID Stability and Reuse

`spawn_blocking` pins the thread for the closure's lifetime. The TID is stable
while the instance runs. When the instance exits, the thread returns to Tokio's
pool and the TID may be reused by a new instance.

**Mitigation:** The Supervisor deregisters the TID immediately when the
instance exits (inside the closure, after `instance.run()` returns). A
periodic cleanup task also removes stale entries where the TID no longer
exists (`kill(tid, 0)` returns `ESRCH`).

---

## Architecture Overview

### The Pull Model

```
┌─────────────────────────────────────────────────────────────────────────┐
│  LAYER 1 — Supervisor (same process as gateway)                        │
│                                                                         │
│  When spawning an instance:                                            │
│  1. gettid() → TID 12346                                               │
│  2. register_tid(12346, {ns="production", app="payments:v1"})          │
│  3. Track: port_to_tid[54321] = 12346                                  │
│                                                                         │
│  Shared maps (in-process, synchronous):                                │
│  • port_to_tid: source_port 54321 → TID 12346                         │
│  • tid_to_identity: TID 12346 → {ns="production", app="payments:v1"}   │
└─────────────────────────────────────────────────────────────────────────┘
                                    │
                                    │  Gateway calls resolve_identity(54321)
                                    │  → port_to_tid[54321] = 12346
                                    │  → tid_to_identity[12346] = {ns="production", ...}
                                    │  → returns CallerIdentity
                                    ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  LAYER 2 — Internal Gateway (same process)                             │
│                                                                         │
│  proxy_handler:                                                         │
│  1. Strip any pre-existing X-Namespace headers (prevent forgery)       │
│  2. Call resolve_identity(peer_addr.port())                            │
│  3. If found: enforce namespace policy (same-ns allow, cross-ns deny)  │
│  4. If not found: deny by default                                       │
│  5. Apply rate limiting, circuit breaker, auth                          │
│  6. Forward to target app                                               │
└─────────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────────┐
│  LAYER 3 — eBPF (Kernel-level, audit + enforcement)                    │
│                                                                         │
│  MONITORED_TIDS map (also writable from userspace):                     │
│  • TID 12346 → {ns="production", app="payments:v1"}                    │
│                                                                         │
│  ns_audit_sendto program:                                               │
│  • Watches sendto/write syscalls from monitored TIDs                    │
│  • Detects forged X-Namespace headers in the buffer                    │
│  • Emits NamespaceAudit events for logging                             │
│                                                                         │
│  SK_MSG enforcement (Linux 5.8+, Phase 4):                              │
│  • Drops packets from unregistered TIDs to port 9080                   │
│  • Defense in depth: even if WASI layer is bypassed, kernel blocks     │
└─────────────────────────────────────────────────────────────────────────┘
```

### Request Flow

```
1. Supervisor spawns AppA on TID 12346
   → register_tid(12346, {ns="production", app="payments:v1"})
   → (TID is stored in ManagedInstance.tid)

2. AppA connects to gateway:9080
   → WASI socket_addr_check: allow (port 9080 is in allowed_ports)
   → Kernel: TCP handshake completes, OS assigns source port 54321
   → eBPF inet_sock_set_state: logs TID 12346 connected to port 9080 (audit)

3. AppA sends HTTP request on the established connection
   → Gateway receives HTTP request from peer port 54321
   → Gateway calls: resolve_identity(54321)
     → port_to_tid[54321] = 12346  (Supervisor populated this)
     → tid_to_identity[12346] = {ns="production", app="payments:v1"}
     → returns CallerIdentity {ns="production", app="payments:v1"}
   → Gateway parses Host: "api.production.internal" → target ns="production"
   → Same namespace → ALLOW
   → Gateway forwards to target app

4. AppA exits
   → deregister_tid(12346)
   → port_to_tid.remove(54321)
   → tid_to_identity.remove(12346)
```

**Why this works:** The gateway and the Supervisor are in the **same process**.
The `port_to_tid` and `tid_to_identity` maps are in-process data structures.
The gateway's query is a synchronous HashMap lookup — no race conditions, no
event ordering issues, no missed events.

---

## Limitations: What eBPF CAN and CANNOT Do

### What eBPF Is Used For

| Protocol | eBPF Capability | Notes |
|----------|----------------|-------|
| HTTP/1.1 cleartext | ✅ Audit (read buffer, detect forged headers) | Can scan for `X-Namespace` in send buffer |
| HTTP/1.1 chunked | ⚠️ Audit only | Chunk boundaries complicate header scanning |
| HTTP/2 | ❌ Cannot parse | Binary framing + HPACK compression |
| HTTPS/TLS | ❌ Cannot parse | Encrypted before `send()` syscall |
| WebSocket | ❌ Cannot parse | Opaque after upgrade |

### Why Tracepoints Are Read-Only

Tracepoints (`sys_enter_sendto`, `sys_enter_write`) are **read-only
observers**. They can inspect syscall arguments but **cannot modify the
userspace buffer**. This means:

- A tracepoint program can **audit** (log what the app sends)
- A tracepoint program **cannot inject** headers into the buffer
- The only eBPF mechanism that can interact with socket data is
  `BPF_PROG_TYPE_SK_MSG` (Linux 5.8+), and even that **cannot prepend data**

**Architectural consequence:** eBPF is the **audit and enforcement** layer,
not the identity propagation layer. Identity propagation happens via
in-process data structures (the Supervisor and gateway share the same
address space). eBPF provides defense in depth: even if a Wasm module escapes
the WASI sandbox, eBPF can still detect and block unauthorized traffic.

### Recommended Protocol for Internal Mesh

Use **cleartext HTTP/1.1** for East-West traffic:

- Apps are on the same node (loopback)
- The loopback interface is trusted
- TLS termination happens at the North-South edge (Pingora)
- eBPF connection tracking works cleanly on cleartext

---

## eBPF Program Design

### Current Codebase Context

The existing `ebpf-monitor` crate uses **aya-ebpf** (Rust BPF programs). The
BPF programs are in `crates/ebpf-monitor/bpf/src/` and share types with
userspace via `crates/ebpf-monitor/bpf/src/common.rs` and
`crates/ebpf-monitor/src/common.rs`.

Existing programs:
- `process_tracker.rs` — `sched_process_exec`/`sched_process_exit` tracepoints
- `tcp_monitor.rs` — `inet_sock_set_state` tracepoint
- `fd_watcher.rs` — `fd_install`/`do_filp_close` kprobes
- `mem_pressure.rs` — `try_to_free_pages` kprobe
- `disk_monitor.rs` — `block_rq_issue`/`block_rq_complete` tracepoints
- `syscall_counter.rs` — `raw_syscalls/sys_enter` tracepoint

All existing programs write to a shared `EVENTS` ring buffer and read from a
shared `CONFIG` array map. We extend this pattern.

### 1. New eBPF Maps

#### `MONITORED_TIDS` — Hash Map: `u32 → TidIdentity`

Maps a Linux Thread ID (TID) to the namespace/app identity of the Wasm
instance running on that thread.

```rust
// crates/ebpf-monitor/bpf/src/namespace_enforcer.rs
// Also declared in crates/ebpf-monitor/src/common.rs (userspace mirror)

/// Identity of a Wasm instance, keyed by its OS Thread ID.
/// Shared between eBPF (kernel) and userspace (Rust) via #[repr(C)].
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct TidIdentity {
    /// Namespace name (UTF-8, null-terminated, 64 bytes max).
    pub namespace: [u8; 64],
    /// App ID (UTF-8, null-terminated, 128 bytes max).
    /// Format: "app-name:v1" (bare_app_name:version).
    pub app_id: [u8; 128],
    /// Timestamp when this TID was registered (bpf_ktime_get_ns()).
    /// Used for stale entry detection.
    pub registered_at_ns: u64,
    /// Bitflags: ENABLED, AUDIT_ONLY.
    pub flags: u32,
    /// Reserved for alignment (struct must be 8-byte aligned).
    pub _padding: u32,
}

/// Bitflags for TidIdentity.flags.
#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TidFlags {
    /// TID is registered and active.
    Enabled = 1 << 0,
    /// Audit-only mode: log but don't enforce.
    AuditOnly = 1 << 1,
}
```

```rust
// In the eBPF program (aya-ebpf):

#[map]
static MONITORED_TIDS: HashMap<u32, TidIdentity> =
    HashMap::with_max_entries(4096, 0);
```

**Why 4096 entries:** A single node realistically runs 100-500 Wasm instances.
4096 is a safe upper bound. If the map is full, `register_tid` returns an
error and the instance falls back to unregistered mode (gateway denies by
default).

#### `NS_ENFORCE_CONFIG` — Array Map: `u32 → NsEnforceConfig` (singleton)

Global configuration for the namespace enforcement subsystem.

```rust
/// Configuration for namespace enforcement eBPF programs.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NsEnforceConfig {
    /// Gateway port to monitor (default: 9080).
    pub gateway_port: u32,
    /// Flags: ENABLE_AUDIT, ENABLE_FORGED_HEADER_DETECT, ENABLE_SK_MSG.
    pub flags: u32,
    /// Node PID (to filter relevant events).
    pub node_pid: u32,
    /// Reserved.
    pub _reserved: u32,
}

/// Bitflags for NsEnforceConfig.flags.
#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum NsEnforceFlags {
    /// Enable audit tracepoint (log gateway-bound traffic).
    EnableAudit = 1 << 0,
    /// Enable forged X-Namespace header detection.
    EnableForgedHeaderDetect = 1 << 1,
    /// Enable SK_MSG enforcement (Linux 5.8+ only).
    EnableSkMsg = 1 << 2,
}
```

```rust
// In the eBPF program (aya-ebpf):

#[map]
static NS_ENFORCE_CONFIG: Array<NsEnforceConfig> =
    Array::with_max_entries(1, 0);
```

### 2. New eBPF Program: `ns_connection_tracker`

This is the **primary identity attribution program**. It extends the existing
`tcp_monitor.rs` pattern to track which TID connects to the gateway port.

```rust
// crates/ebpf-monitor/bpf/src/namespace_enforcer.rs

#![no_std]
#![no_main]

use aya_ebpf::{
    cty::c_long,
    macros::{map, tracepoint},
    maps::{Array, HashMap, RingBuf},
    programs::TracePointContext,
};
use ebpf_monitor_bpf_common::*;

#[map]
static CONFIG: Array<MonitorConfigMap> = Array::with_max_entries(1, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(1024 * 1024, 0);

#[map]
static MONITORED_TIDS: HashMap<u32, TidIdentity> =
    HashMap::with_max_entries(4096, 0);

#[map]
static NS_ENFORCE_CONFIG: Array<NsEnforceConfig> =
    Array::with_max_entries(1, 0);

/// Tracepoint: sock/inet_sock_set_state
///
/// Extends the existing tcp_monitor to track which monitored TIDs
/// connect to the gateway port. When a monitored TID establishes a
/// TCP connection to port 9080, we emit a TidConnection event so the
/// gateway can attribute the connection to a namespace.
///
/// This is the SAME tracepoint as tcp_monitor.rs uses. In production,
/// both programs attach to the same tracepoint. Alternatively, the
/// namespace tracking logic can be merged into tcp_monitor.rs.
#[tracepoint]
pub fn ns_inet_sock_set_state(ctx: TracePointContext) -> c_long {
    match try_ns_inet_sock_set_state(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_ns_inet_sock_set_state(ctx: TracePointContext) -> Result<c_long, c_long> {
    let ns_config = NS_ENFORCE_CONFIG.get(0).ok_or(0)?;
    let config = CONFIG.get(0).ok_or(0)?;

    // Read tracepoint arguments (same layout as tcp_monitor.rs):
    //   offset 8:  old_state (u32)
    //   offset 12: new_state (u32)
    //   offset 16: src_port (u16)
    //   offset 18: dst_port (u16)
    let old_state: u32 = unsafe { ctx.read_at(8)? }.ok_or(0)?;
    let new_state: u32 = unsafe { ctx.read_at(12)? }.ok_or(0)?;
    let _src_port: u16 = unsafe { ctx.read_at(16)? }.ok_or(0)?;
    let dst_port: u16 = unsafe { ctx.read_at(18)? }.ok_or(0)?;

    let pid_tgid = unsafe { aya_ebpf::helpers::bpf_get_current_pid_tgid() };
    let pid = (pid_tgid >> 32) as u32;
    let tid = (pid_tgid & 0xFFFFFFFF) as u32;

    // Only monitor the wasm-node process
    if pid != config.node_pid {
        return Ok(0);
    }

    // Only care about connections to the gateway port
    if dst_port as u32 != ns_config.gateway_port {
        return Ok(0);
    }

    // Check if this TID is registered
    let tid_identity = match MONITORED_TIDS.get(&tid) {
        Some(id) => id,
        None => {
            // Unregistered TID connecting to gateway — emit warning
            if new_state == TCP_ESTABLISHED {
                let event = NamespaceAuditEvent {
                    header: EventHeader {
                        event_type: EventType::NamespaceAudit as u32,
                        timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
                        pid,
                        tid,
                    },
                    audit_type: NamespaceAuditType::UnregisteredTid as u32,
                    source_namespace: [0u8; 64],
                    source_app_id: [0u8; 128],
                    dest_port: dst_port,
                    source_port: 0, // filled below for established
                    _padding: 0,
                };
                EVENTS.output(&event, 0);
            }
            return Ok(0);
        }
    };

    // TCP_ESTABLISHED: a monitored TID connected to the gateway
    if new_state == TCP_ESTABLISHED {
        // We need the source port. The tracepoint gives us dst_port
        // but src_port is the local ephemeral port. For a connection
        // TO the gateway, src_port is the ephemeral port the OS assigned.
        // The tracepoint's "sport" field is the source port of the
        // TCP socket (which is the ephemeral port for outbound connections).
        let source_port: u16 = unsafe { ctx.read_at(16)? }.ok_or(0)?;

        let event = NamespaceAuditEvent {
            header: EventHeader {
                event_type: EventType::TidConnection as u32,
                timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
                pid,
                tid,
            },
            audit_type: NamespaceAuditType::ConnectionEstablished as u32,
            source_namespace: tid_identity.namespace,
            source_app_id: tid_identity.app_id,
            dest_port: dst_port,
            source_port,
            _padding: 0,
        };
        EVENTS.output(&event, 0);
    }

    // TCP_CLOSE: connection to gateway closed — remove from identity table
    if new_state == TCP_CLOSE {
        let source_port: u16 = unsafe { ctx.read_at(16)? }.ok_or(0)?;
        let event = NamespaceAuditEvent {
            header: EventHeader {
                event_type: EventType::TidDisconnection as u32,
                timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
                pid,
                tid,
            },
            audit_type: NamespaceAuditType::ConnectionClosed as u32,
            source_namespace: tid_identity.namespace,
            source_app_id: tid_identity.app_id,
            dest_port: dst_port,
            source_port,
            _padding: 0,
        };
        EVENTS.output(&event, 0);
    }

    Ok(0)
}

const TCP_ESTABLISHED: u32 = 1;
const TCP_CLOSE: u32 = 7;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}
```

### 3. New eBPF Program: `ns_audit_sendto`

Audits `sendto`/`write` syscalls from monitored TIDs to detect forged headers.

```rust
// crates/ebpf-monitor/bpf/src/namespace_enforcer.rs (continued)

/// Tracepoint: syscalls:sys_enter_sendto
///
/// Fires when any thread calls sendto(). We check:
/// 1. Is the calling TID registered in MONITORED_TIDS?
/// 2. Is the buffer an HTTP request?
/// 3. Does the buffer contain a forged X-Namespace header?
#[tracepoint]
pub fn ns_audit_sendto(ctx: TracePointContext) -> c_long {
    match try_ns_audit_sendto(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_ns_audit_sendto(ctx: TracePointContext) -> Result<c_long, c_long> {
    let ns_config = NS_ENFORCE_CONFIG.get(0).ok_or(0)?;
    if ns_config.flags & NsEnforceFlags::EnableAudit as u32 == 0 {
        return Ok(0);
    }

    let pid_tgid = unsafe { aya_ebpf::helpers::bpf_get_current_pid_tgid() };
    let pid = (pid_tgid >> 32) as u32;
    let tid = (pid_tgid & 0xFFFFFFFF) as u32;

    // Only monitor the wasm-node process
    if pid != ns_config.node_pid {
        return Ok(0);
    }

    // Is this TID registered?
    let tid_identity = MONITORED_TIDS.get(&tid).ok_or(0)?;
    if tid_identity.flags & TidFlags::Enabled as u32 == 0 {
        return Ok(0);
    }

    // Read syscall arguments
    let args = ctx.args();
    if args.len() < 3 {
        return Ok(0);
    }

    let buf_ptr = args[1] as *const u8;
    let buf_len = args[2] as usize;

    if buf_len < 16 {
        return Ok(0);
    }

    // Check if the buffer looks like an HTTP request
    let mut method = [0u8; 8];
    unsafe {
        let _ = aya_ebpf::helpers::bpf_probe_read_user_str(
            method.as_mut_ptr() as *mut u8, 8, buf_ptr,
        );
    }

    let is_http = method.starts_with(b"GET ")
        || method.starts_with(b"POST")
        || method.starts_with(b"PUT ")
        || method.starts_with(b"DELE")
        || method.starts_with(b"PATC")
        || method.starts_with(b"HEAD");

    if !is_http {
        return Ok(0);
    }

    // Scan for forged X-Namespace headers
    if ns_config.flags & NsEnforceFlags::EnableForgedHeaderDetect as u32 != 0 {
        let scan_len = buf_len.min(1024);
        let mut header_buf = [0u8; 1024];
        unsafe {
            let _ = aya_ebpf::helpers::bpf_probe_read_user(
                header_buf.as_mut_ptr() as *mut u8, scan_len, buf_ptr,
            );
        }

        // Search for "X-Namespace" (case-insensitive)
        for i in 0..scan_len.saturating_sub(12) {
            if i + 11 <= scan_len {
                let candidate = &header_buf[i..i+11];
                if candidate.eq_ignore_ascii_case(b"X-Namespace") {
                    // FORGED HEADER DETECTED — the app should never send this
                    let event = NamespaceAuditEvent {
                        header: EventHeader {
                            event_type: EventType::NamespaceForgedHeader as u32,
                            timestamp_ns: unsafe {
                                aya_ebpf::helpers::bpf_ktime_get_ns()
                            },
                            pid,
                            tid,
                        },
                        audit_type: NamespaceAuditType::ForgedHeader as u32,
                        source_namespace: tid_identity.namespace,
                        source_app_id: tid_identity.app_id,
                        dest_port: 0,
                        source_port: 0,
                        _padding: 0,
                    };
                    EVENTS.output(&event, 0);
                    break;
                }
            }
        }
    }

    // Emit audit event for this gateway-bound request
    let event = NamespaceAuditEvent {
        header: EventHeader {
            event_type: EventType::NamespaceAudit as u32,
            timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
            pid,
            tid,
        },
        audit_type: NamespaceAuditType::GatewayRequest as u32,
        source_namespace: tid_identity.namespace,
        source_app_id: tid_identity.app_id,
        dest_port: 0,
        source_port: 0,
        _padding: 0,
    };
    EVENTS.output(&event, 0);

    Ok(0)
}
```

### 4. New Event Types

Extend the existing `EventType` enum in `common.rs`:

```rust
// crates/ebpf-monitor/src/common.rs — additions to EventType

#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum EventType {
    // Existing (1-10):
    ProcessExec = 1,
    ProcessExit = 2,
    TcpConnect = 3,
    TcpClose = 4,
    TcpRetransmit = 5,
    FdOpen = 6,
    MemPressure = 7,
    DiskSlowIo = 8,
    SyscallAnomaly = 9,
    FdLimitApproaching = 10,
    // ── Namespace enforcement events ──
    /// Monitored TID connected to gateway port.
    TidConnection = 11,
    /// Monitored TID disconnected from gateway port.
    TidDisconnection = 12,
    /// Audit: gateway-bound HTTP request from monitored TID.
    NamespaceAudit = 13,
    /// Security: forged X-Namespace header detected.
    NamespaceForgedHeader = 14,
}
```

```rust
/// Namespace enforcement event sent from eBPF to userspace.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct NamespaceAuditEvent {
    pub header: EventHeader,
    /// Type of event (NamespaceAuditType enum).
    pub audit_type: u32,
    /// Source namespace (UTF-8, null-terminated, 64 bytes).
    pub source_namespace: [u8; 64],
    /// Source app ID (UTF-8, null-terminated, 128 bytes).
    pub source_app_id: [u8; 128],
    /// Destination port (gateway port).
    pub dest_port: u16,
    /// Source port (ephemeral port for outbound connections).
    pub source_port: u16,
    pub _padding: u32,
}

/// Types of namespace audit events.
#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum NamespaceAuditType {
    /// Normal gateway-bound HTTP request from a monitored TID.
    GatewayRequest = 0,
    /// Monitored TID established TCP connection to gateway.
    ConnectionEstablished = 1,
    /// Monitored TID's TCP connection to gateway closed.
    ConnectionClosed = 2,
    /// App sent pre-existing X-Namespace header (forged).
    ForgedHeader = 3,
    /// Unregistered TID connected to gateway port.
    UnregisteredTid = 4,
}
```

---

## Data Structures (Userspace Rust)

### 1. `TidIdentity` (Shared with eBPF)

```rust
// crates/ebpf-monitor/src/common.rs — additions

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct TidIdentity {
    pub namespace: [u8; 64],
    pub app_id: [u8; 128],
    pub registered_at_ns: u64,
    pub flags: u32,
    pub _padding: u32,
}

impl TidIdentity {
    pub fn new(namespace: &str, app_id: &str) -> Self {
        let mut s = Self {
            namespace: [0u8; 64],
            app_id: [0u8; 128],
            registered_at_ns: 0,
            flags: TidFlags::Enabled as u32,
            _padding: 0,
        };
        s.set_namespace(namespace);
        s.set_app_id(app_id);
        s
    }

    pub fn set_namespace(&mut self, s: &str) {
        let bytes = s.as_bytes();
        let len = bytes.len().min(63);
        self.namespace[..len].copy_from_slice(&bytes[..len]);
        self.namespace[len] = 0;
    }

    pub fn set_app_id(&mut self, s: &str) {
        let bytes = s.as_bytes();
        let len = bytes.len().min(127);
        self.app_id[..len].copy_from_slice(&bytes[..len]);
        self.app_id[len] = 0;
    }

    pub fn namespace_str(&self) -> &str {
        let len = self.namespace.iter().position(|&b| b == 0).unwrap_or(64);
        std::str::from_utf8(&self.namespace[..len]).unwrap_or("invalid")
    }

    pub fn app_id_str(&self) -> &str {
        let len = self.app_id.iter().position(|&b| b == 0).unwrap_or(128);
        std::str::from_utf8(&self.app_id[..len]).unwrap_or("invalid")
    }
}

#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TidFlags {
    Enabled = 1 << 0,
    AuditOnly = 1 << 1,
}

#[cfg(feature = "ebpf")]
unsafe impl aya::Pod for TidIdentity {}
#[cfg(feature = "ebpf")]
unsafe impl aya::Pod for NsEnforceConfig {}
#[cfg(feature = "ebpf")]
unsafe impl aya::Pod for NamespaceAuditEvent {}
```

### 2. `MonitorEvent` Extensions

```rust
// crates/ebpf-monitor/src/actions.rs — additions to MonitorEvent enum

pub enum MonitorEvent {
    // ... existing variants ...

    /// Monitored TID connected to gateway port.
    TidConnection {
        tid: u32,
        namespace: String,
        app_id: String,
        source_port: u16,
    },

    /// Monitored TID disconnected from gateway port.
    TidDisconnection {
        tid: u32,
        source_port: u16,
    },

    /// Audit: gateway-bound HTTP request from monitored TID.
    NamespaceAudit {
        tid: u32,
        namespace: String,
        app_id: String,
    },

    /// Security: forged X-Namespace header detected.
    NamespaceForgedHeader {
        tid: u32,
        namespace: String,
        app_id: String,
    },

    /// Unregistered TID connected to gateway port.
    UnregisteredTidConnection {
        tid: u32,
    },
}
```

### 3. `RecoveryAction` Extensions

```rust
// crates/ebpf-monitor/src/actions.rs — additions to RecoveryAction enum

pub enum RecoveryAction {
    // ... existing variants ...

    /// A namespace enforcement security incident was detected.
    NamespaceSecurityIncident {
        tid: u32,
        namespace: String,
        app_id: String,
        incident_type: NamespaceIncidentType,
    },
}

#[derive(Debug, Clone)]
pub enum NamespaceIncidentType {
    /// App sent a pre-existing X-Namespace header (attempted forgery).
    ForgedHeader,
    /// Unregistered TID connected to gateway port.
    UnregisteredTidAccess,
}
```

---

## ebpf-monitor Crate API Extensions

### New Module: `namespace_map.rs`

```rust
// crates/ebpf-monitor/src/namespace_map.rs

//! Userspace API for the MONITORED_TIDS eBPF map.
//!
//! Provides `register_tid` and `deregister_tid` operations that the
//! Supervisor calls when spawning/killing instances.
//! Also provides the `resolve_identity` method that the gateway calls
//! to answer "who is calling from source port X?"

use crate::common::{TidIdentity, TidFlags};
use tracing::{info, warn};

/// Identity of a caller, returned by `resolve_identity()`.
#[derive(Debug, Clone)]
pub struct CallerIdentity {
    pub namespace: String,
    pub app_id: String,
    pub tid: u32,
}

/// Handle to the MONITORED_TIDS eBPF map + in-process identity tables.
///
/// This is the single source of truth for identity resolution. The gateway
/// calls `resolve_identity(source_port)` and gets back a `CallerIdentity`.
///
/// Two maps are maintained:
/// - `tid_to_identity`: TID → {namespace, app_id} (populated by register_tid)
/// - `port_to_tid`: source_port → TID (populated when connections are tracked)
///
/// When the `ebpf` feature is not enabled, all operations use in-process
/// fallback maps. The gateway reads from the same maps for identity resolution.
pub struct NamespaceMap {
    #[cfg(feature = "ebpf")]
    inner: Option<aya::maps::HashMap<aya::maps::HashMap<u32, TidIdentity>>>,
    /// TID → TidIdentity. Always maintained. Primary identity store.
    tid_to_identity: std::sync::RwLock<std::collections::HashMap<u32, TidIdentity>>,
    /// Source port → TID. Populated by the Supervisor when it detects
    /// which source port a TID is using for its gateway connection.
    port_to_tid: std::sync::RwLock<std::collections::HashMap<u16, u32>>,
}
}

impl NamespaceMap {
    /// Create from a loaded eBPF object.
    #[cfg(feature = "ebpf")]
    pub fn from_ebpf(ebpf: &mut aya::Bpf) -> Self {
        let inner = match ebpf.map_mut("MONITORED_TIDS") {
            Some(map) => {
                match aya::maps::HashMap::<_, u32, TidIdentity>::try_from(map) {
                    Ok(hash_map) => {
                        info!("MONITORED_TIDS eBPF map opened");
                        Some(hash_map)
                    }
                    Err(e) => {
                        warn!(error = %e, "MONITORED_TIDS map wrong type — using fallback");
                        None
                    }
                }
            }
            None => {
                warn!("MONITORED_TIDS map not found — using fallback");
                None
            }
        };

        NamespaceMap {
            #[cfg(feature = "ebpf")]
            inner,
            tid_to_identity: std::sync::RwLock::new(std::collections::HashMap::new()),
            port_to_tid: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Create a fallback-only map (no eBPF).
    pub fn new_fallback() -> Self {
        NamespaceMap {
            #[cfg(feature = "ebpf")]
            inner: None,
            tid_to_identity: std::sync::RwLock::new(std::collections::HashMap::new()),
            port_to_tid: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Register a TID with its namespace/app identity.
    ///
    /// Called from inside the `spawn_blocking` closure, after `gettid()`
    /// but before `instance.run()`.
    pub fn register_tid(&self, tid: u32, mut identity: TidIdentity) -> Result<(), String> {
        identity.flags = TidFlags::Enabled as u32;
        identity.registered_at_ns = Self::now_ns();

        #[cfg(feature = "ebpf")]
        if let Some(ref map) = self.inner {
            match map.insert(tid, identity, 0) {
                Ok(()) => {
                    info!(tid, ns = identity.namespace_str(), app = identity.app_id_str(),
                          "TID registered in eBPF map");
                    self.tid_to_identity.write().unwrap().insert(tid, identity);
                    return Ok(());
                }
                Err(e) => {
                    warn!(tid, error = %e, "eBPF map insert failed — using fallback");
                }
            }
        }

        self.tid_to_identity.write().unwrap().insert(tid, identity);
        info!(tid, ns = identity.namespace_str(), app = identity.app_id_str(),
              "TID registered");
        Ok(())
    }

    /// Deregister a TID from the map.
    ///
    /// Called when an instance is killed or exits.
    /// Also removes any port_to_tid entries for this TID.
    pub fn deregister_tid(&self, tid: u32) -> Result<(), String> {
        #[cfg(feature = "ebpf")]
        if let Some(ref map) = self.inner {
            let _ = map.remove(&tid);
        }

        self.tid_to_identity.write().unwrap().remove(&tid);

        // Remove any port→TID mappings for this TID
        let mut port_map = self.port_to_tid.write().unwrap();
        port_map.retain(|_, &mut t| t != tid);

        info!(tid, "TID deregistered");
        Ok(())
    }

    /// Bind a source port to a TID.
    ///
    /// Called when the Supervisor detects that a TID has established a
    /// TCP connection to the gateway from a specific source port.
    /// The gateway will later query: "who is on source port X?"
    pub fn bind_port(&self, source_port: u16, tid: u32) {
        self.port_to_tid.write().unwrap().insert(source_port, tid);
        tracing::debug!(source_port, tid, "Port bound to TID");
    }

    /// Unbind a source port (when the connection closes).
    pub fn unbind_port(&self, source_port: u16) {
        self.port_to_tid.write().unwrap().remove(&source_port);
        tracing::debug!(source_port, "Port unbound");
    }

    /// Resolve the identity of a caller by source port.
    ///
    /// This is the method the gateway calls for each incoming request.
    /// It performs a two-step lookup: source_port → TID → {namespace, app_id}.
    ///
    /// Returns `None` if the source port is not bound or the TID is not
    /// registered (unregistered connection — deny by default).
    pub fn resolve_identity(&self, source_port: u16) -> Option<CallerIdentity> {
        let port_map = self.port_to_tid.read().unwrap();
        let tid = port_map.get(&source_port).copied()?;
        drop(port_map);

        let tid_map = self.tid_to_identity.read().unwrap();
        let identity = tid_map.get(&tid)?;

        Some(CallerIdentity {
            namespace: identity.namespace_str().to_string(),
            app_id: identity.app_id_str().to_string(),
            tid,
        })
    }

    /// Look up a TID's identity directly (for admin/debug API).
    pub fn lookup_tid(&self, tid: u32) -> Option<TidIdentity> {
        self.tid_to_identity.read().unwrap().get(&tid).copied()
    }

    /// Cleanup stale TIDs whose threads no longer exist.
    ///
    /// Called periodically by the Supervisor's health loop.
    pub fn cleanup_stale_tids(&self) -> usize {
        let tids: Vec<u32> = self.tid_to_identity.read().unwrap().keys().copied().collect();
        let mut removed = 0;

        for tid in tids {
            if !Self::is_tid_alive(tid) {
                warn!(tid, "Cleaning up stale TID");
                let _ = self.deregister_tid(tid);
                removed += 1;
            }
        }

        removed
    }

    #[cfg(target_os = "linux")]
    fn is_tid_alive(tid: u32) -> bool {
        unsafe {
            let ret = libc::kill(tid as i32, 0);
            ret == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn is_tid_alive(_tid: u32) -> bool { true }

    #[cfg(target_os = "linux")]
    fn now_ns() -> u64 {
        let mut ts = std::mem::MaybeUninit::<libc::timespec>::uninit();
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, ts.as_mut_ptr());
            let ts = ts.assume_init();
            (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn now_ns() -> u64 {
        use std::time::Instant;
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let start = START.get_or_init(Instant::now);
        start.elapsed().as_nanos() as u64
    }
}
```

### Changes to `lib.rs`

```rust
// crates/ebpf-monitor/src/lib.rs — additions

pub mod namespace_map;

pub use namespace_map::{NamespaceMap, CallerIdentity};
```

**No `connection_table` module needed.** The `NamespaceMap` already contains
both `tid_to_identity` and `port_to_tid` maps. The gateway calls
`namespace_map.resolve_identity(source_port)` — a single method that does the
two-step lookup synchronously. No async, no events, no separate table.

### Changes to `MonitorHandle`

```rust
// Add to MonitorHandle struct:

pub struct MonitorHandle {
    // ... existing fields ...
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    ebpf_active: bool,
    _consumer_handle: Option<tokio::task::JoinHandle<()>>,
    _fallback_handle: Option<tokio::task::JoinHandle<()>>,
    metrics: Arc<EbpfMetrics>,
    dispatcher: Arc<ActionDispatcher>,

    // ── New: Namespace enforcement ──
    /// MONITORED_TIDS map handle (for register/deregister TID).
    /// Also provides resolve_identity() for the gateway.
    pub namespace_map: Arc<NamespaceMap>,
}
```

### Consumer Extension: Processing Namespace Events

The ring buffer consumer handles namespace audit events for **logging only**.
It does NOT update any connection table — the gateway resolves identity by
querying `NamespaceMap.resolve_identity()` directly.

```rust
// crates/ebpf-monitor/src/consumer.rs — additions to parse_event

// In the match on event_type, add cases for:
//   EventType::NamespaceAudit
//   EventType::NamespaceForgedHeader

// These events are forwarded to the action dispatcher for logging and
// security incident handling. The gateway does NOT consume these events
// for identity resolution — it queries the NamespaceMap directly.
```

---

## Supervisor Integration

### 1. `ManagedInstance` Extension

```rust
// crates/supervisor/src/instance.rs — additions

pub struct ManagedInstance {
    pub id: InstanceId,
    pub app_id: AppId,
    pub addr: SocketAddr,
    pub state: InstanceState,
    pub spawned_at: Instant,
    pub last_request_at: Instant,
    pub request_count: u64,
    pub task: JoinHandle<ExecutionStats>,
    pub shutdown_tx: oneshot::Sender<()>,
    pub billing_info: BillingInfo,

    // ── New: eBPF namespace enforcement ──
    /// OS Thread ID for eBPF registration.
    /// Set from inside the spawn_blocking closure via gettid().
    /// None if TID registration failed.
    pub tid: Option<u32>,
}
```

### 2. `spawn()` Extension

The Supervisor's `spawn()` method registers the TID inside the
`spawn_blocking` closure and communicates the TID back to the async context
so it can track source ports:

```rust
// crates/supervisor/src/lib.rs — modified spawn_blocking closure

let namespace_map = self.namespace_map.clone(); // Arc<NamespaceMap>
let qualified_app_id_clone = qualified_app_id.clone();
let (tid_tx, tid_rx) = tokio::sync::oneshot::channel::<u32>();

let task = tokio::task::spawn_blocking(move || {
    // Get the OS Thread ID for this blocking task
    let tid = gettid();

    // Register TID with identity map
    let identity = TidIdentity::new(
        qualified_app_id_clone.namespace(),
        &qualified_app_id_clone.0,
    );
    if let Err(e) = namespace_map.register_tid(tid, identity) {
        tracing::warn!(tid, error = %e,
            "Failed to register TID — gateway will not attribute this instance");
    }

    // Send TID back to async context so Supervisor can track source ports
    let _ = tid_tx.send(tid);

    let mut instance = match prepared_clone.spawn_instance(
        env_vars, host_port, Some(socket_addr_check)
    ) {
        Ok(instance) => instance,
        Err(e) => {
            // Deregister TID on spawn failure
            let _ = namespace_map.deregister_tid(tid);
            let _ = spawn_result_tx.send(Err(PlatformError::runtime(format!(
                "Failed to spawn instance: {}", e
            ))));
            return ExecutionStats { /* zero stats */ };
        }
    };

    let _ = spawn_result_tx.send(Ok(()));
    let stats = instance.run();

    // Deregister TID on exit
    let _ = namespace_map.deregister_tid(tid);

    stats
});

// After spawn_result_rx confirms success, receive the TID:
let instance_tid = tid_rx.await.ok();
```

### 2b. Source Port Tracking

When a Wasm instance connects to the gateway, the OS assigns an ephemeral
source port. The Supervisor needs to bind this source port to the TID so
the gateway can resolve the caller's identity.

**How to discover the source port:** The Supervisor can read `/proc/net/tcp`
(or use `getsockname()` on the gateway's accepted socket) to find which
source port a TID is using. Alternatively, the `socket_addr_check` callback
already fires at connect time and knows the source port:

```rust
// In the socket_addr_check closure, when the app connects to port 9080:
if dest.port() == common::INTERNAL_GATEWAY_PORT {
    // The app is connecting to the gateway. We know the source app's TID
    // (it's the current thread). We can bind the source port to the TID.
    //
    // However, socket_addr_check only receives the DESTINATION address,
    // not the source port. The source port is assigned by the OS after
    // the connect() syscall completes.
    //
    // Better approach: the gateway knows the source port from peer_addr.
    // It calls namespace_map.resolve_identity(peer_addr.port()).
    // The port_to_tid map is populated by a different mechanism.
}
```

**Better approach — the gateway tells the Supervisor:**

When the gateway accepts a connection, it already knows the source port
(from `peer_addr`). But it doesn't know which TID owns that source port.
The Supervisor needs to populate `port_to_tid` **before** the gateway
receives the first HTTP request on that connection.

**Simplest solution:** Use the existing `NamespaceRegistry.bind_source_port()`
pattern. The Supervisor already calls `bind_source_port(host_port, app_id)`
for the instance's **bind** port. We extend this to also track gateway-bound
source ports.

**Practical approach:** Since the Supervisor and gateway are in the same
process, the Supervisor can monitor `/proc/<pid>/net/tcp` periodically to
discover which source ports are connected to port 9080, and cross-reference
with the TID information from `/proc/<pid>/task/<tid>/fd/`.

**Simplest practical approach:** The `socket_addr_check` callback fires at
connect time. We modify it to also record the source port → TID mapping.
The callback runs on the same thread as the Wasm instance, so `gettid()`
returns the correct TID:

```rust
// Modified socket_addr_check — when connecting to gateway port:
if dest.port() == common::INTERNAL_GATEWAY_PORT {
    // Record that this TID is connecting to the gateway.
    // The source port will be assigned by the OS, but we can
    // discover it after the connect completes.
    //
    // Approach: After socket_addr_check returns true and the connect()
    // completes, the socket has a source port. We can read it from
    // /proc/net/tcp or by monitoring the TCP state changes.
    //
    // For now: the eBPF ns_inet_sock_set_state program detects the
    // connection and emits a TidConnection event with the source port.
    // The consumer updates port_to_tid in the NamespaceMap.
}
```

**Final approach — eBPF populates port_to_tid:**

The eBPF `ns_inet_sock_set_state` program detects when a monitored TID
establishes a TCP connection to port 9080. It emits a `TidConnection` event
with `{tid, source_port}`. The consumer calls
`namespace_map.bind_port(source_port, tid)`. The gateway then calls
`namespace_map.resolve_identity(source_port)`.

This is the **one place** where eBPF events feed into the identity map — not
for the TID→identity mapping (that's done by the Supervisor at spawn time),
but for the port→TID mapping (which only the kernel can observe reliably).

### 3. `kill_instance_internal()` Extension

```rust
// crates/supervisor/src/lib.rs — modified kill_instance_internal()

async fn kill_instance_internal(&self, pool: &mut InstancePool, app_id: &AppId, id: &InstanceId) {
    if let Some(pos) = pool.instances.iter().position(|i| i.id == *id) {
        let inst = pool.instances.remove(pos);

        // ── New: Deregister TID from eBPF map ──
        if let Some(tid) = inst.tid {
            if let Some(ref namespace_map) = self.namespace_map {
                let _ = namespace_map.deregister_tid(tid);
            }
        }

        // ... existing cleanup code (upstream_registry, service_registry, etc.) ...
    }
}
```

### 4. Stale TID Cleanup

Add a periodic cleanup to the health loop:

```rust
// In Supervisor::health_tick()

// ── New: Cleanup stale TIDs ──
if let Some(ref namespace_map) = self.namespace_map {
    let removed = namespace_map.cleanup_stale_tids();
    if removed > 0 {
        tracing::info!(removed, "Cleaned up stale TIDs from eBPF map");
    }
}
```

### 5. Supervisor Struct Extension

```rust
// crates/supervisor/src/lib.rs — additions to Supervisor struct

pub struct Supervisor {
    // ... existing fields ...

    /// eBPF namespace map for TID registration.
    namespace_map: Option<Arc<NamespaceMap>>,

    /// Connection identity table shared with the internal gateway.
    connection_table: Option<Arc<ConnectionTable>>,
}
```

---

## Internal Gateway Changes

### 1. How Identity Resolution Works

The gateway calls `namespace_map.resolve_identity(source_port)` for each
incoming request. This is a **synchronous in-process call** — no events, no
async, no race conditions.

```
1. AppA (TID 12346, ns=production) connects to gateway:9080
   → OS assigns ephemeral source port 54321
   → eBPF detects connection, consumer calls namespace_map.bind_port(54321, 12346)
   → port_to_tid[54321] = 12346
   → tid_to_identity[12346] = {ns="production", app="payments:v1"} (set at spawn time)

2. AppA sends HTTP request on the same TCP connection
   → Gateway sees: peer_addr = 127.0.0.1:54321
   → Gateway calls: namespace_map.resolve_identity(54321)
     → port_to_tid[54321] = 12346
     → tid_to_identity[12346] = {ns="production", app="payments:v1"}
     → returns CallerIdentity {ns="production", app="payments:v1"}
   → Gateway enforces namespace policy based on this identity
```

### 2. Trust Model

| Condition | Trust Level | Reason |
|-----------|-------------|--------|
| Source port resolved via NamespaceMap | **Full trust** | Same-process lookup, TID registered by Supervisor |
| Source port not resolved, eBPF active | **No trust** | Unregistered connection — deny by default |
| Source port not resolved, eBPF not active | **Fallback** | Use `NamespaceRegistry.port_to_app` (TOCTOU-vulnerable) |
| Connection from non-loopback | **Never trust** | External traffic must not carry internal identity |

### 3. Header Stripping (Critical Security Measure)

**Before any identity resolution, the gateway MUST strip all pre-existing
`X-Namespace`, `X-Source-App`, and `X-Source-Tid` headers from the request.**

This prevents header reflection attacks where a malicious app sends forged
identity headers. The gateway never reads identity from headers — it resolves
identity from the connection table. Any identity headers in the request are
either:
- Left over from a previous hop (should not exist on loopback)
- Injected by the app (forgery attempt)

Both cases are handled by stripping.

### 4. Handler Update

```rust
// crates/internal_gateway/src/lib.rs — modified proxy_handler

/// Identity resolved from the connection table.
#[derive(Debug, Clone)]
struct CallerIdentity {
    namespace: String,
    app_id: String,
    tid: u32,
}

async fn proxy_handler(
    State(gw): State<Arc<InternalGateway>>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    mut headers: HeaderMap,
    req: Request<axum::body::Body>,
) -> Result<Response<axum::body::Body>, StatusCode> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // ── 0. STRIP all internal identity headers ────────────────────────
    // Prevent header reflection attacks. The gateway resolves identity
    // from the connection table, not from headers.
    headers.remove("x-namespace");
    headers.remove("x-source-app");
    headers.remove("x-source-tid");

    // ── 1. RESOLVE caller identity — ask the NamespaceMap ────────────
    let caller_identity = if peer_addr.ip().is_loopback() {
        if let Some(ref ns_map) = gw.namespace_map {
            match ns_map.resolve_identity(peer_addr.port()) {
                Some(identity) => {
                    tracing::info!(
                        source_port = peer_addr.port(),
                        namespace = %identity.namespace,
                        app_id = %identity.app_id,
                        tid = identity.tid,
                        "[INTERNAL-GW] caller identity resolved"
                    );
                    Some(identity)
                }
                None => {
                    // Source port not in port_to_tid map
                    if gw.ebpf_active {
                        // eBPF is active but this connection is unregistered — deny
                        tracing::warn!(
                            source_port = peer_addr.port(),
                            "[INTERNAL-GW] unregistered connection — denying"
                        );
                        return Err(StatusCode::UNAUTHORIZED);
                    } else {
                        // eBPF not active — fall back to port_to_app (TOCTOU-vulnerable)
                        tracing::debug!(
                            source_port = peer_addr.port(),
                            "[INTERNAL-GW] eBPF inactive, falling back to port_to_app"
                        );
                        gw.registry.resolve_source_app(peer_addr.port()).await
                            .map(|app_id| CallerIdentity {
                                namespace: app_id.namespace().to_string(),
                                app_id: app_id.0.clone(),
                                tid: 0,
                            })
                    }
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // ── 2. TARGET from the Host header ────────────────────────────────
    let host_header = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let (target_app_name, target_namespace) =
        parse_internal_host(host_header).ok_or_else(|| {
            tracing::warn!(host = %host_header, "[INTERNAL-GW] invalid internal host format");
            StatusCode::BAD_REQUEST
        })?;

    // ── 3. NAMESPACE CHECK ───────────────────────────────────────────
    if let Some(ref caller) = caller_identity {
        if caller.namespace != target_namespace {
            // Cross-namespace call — check allowlist
            if !is_cross_namespace_allowed(&caller.namespace, &target_namespace, &gw.gateway_config) {
                tracing::warn!(
                    caller_ns = %caller.namespace,
                    target_ns = %target_namespace,
                    caller_app = %caller.app_id,
                    "[INTERNAL-GW] cross-namespace call DENIED"
                );
                return Err(StatusCode::FORBIDDEN);
            }
            tracing::info!(
                caller_ns = %caller.namespace,
                target_ns = %target_namespace,
                "[INTERNAL-GW] cross-namespace call ALLOWED (allowlist)"
            );
        }
    } else {
        // No identity — deny by default
        if !gw.allow_anonymous_internal {
            tracing::warn!("[INTERNAL-GW] denying anonymous request");
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    // ── 4. RATE LIMITING ─────────────────────────────────────────────
    if let Some(ref caller) = caller_identity {
        if !gw.rate_limiter.check_request(&caller.app_id).await {
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
    }

    // ── 5. RESOLVE target ────────────────────────────────────────────
    let target_addr = gw.registry.resolve(&target_namespace, target_app_name)
        .await
        .ok_or_else(|| {
            tracing::warn!(target_ns = %target_namespace, target_app = %target_app_name,
                           "[INTERNAL-GW] target not found");
            StatusCode::NOT_FOUND
        })?;

    // ── 6. CIRCUIT BREAKER + FORWARD ─────────────────────────────────
    // (existing forwarding logic)
}
```

### 5. Gateway Struct Extension

```rust
// crates/internal_gateway/src/lib.rs — additions to InternalGateway

pub struct InternalGateway {
    // ... existing fields ...
    pub registry: Arc<supervisor::network::NamespaceRegistry>,
    pub rate_limiter: Arc<proxy::rate_limiter::RateLimiter>,
    pub circuit_breaker: Arc<proxy::gateway::circuit_breaker::CircuitBreakerManager>,
    pub gateway_config: Arc<proxy::gateway::Gateway>,
    pub http_client: reqwest::Client,

    // ── New: Namespace enforcement ──
    /// Namespace identity map — gateway calls resolve_identity(source_port).
    pub namespace_map: Option<Arc<ebpf_monitor::NamespaceMap>>,
    /// Whether eBPF namespace enforcement is active.
    pub ebpf_active: bool,
    /// Whether to allow anonymous (unidentified) internal requests.
    pub allow_anonymous_internal: bool,
}
```

### 6. Cross-Namespace Allowlist

```rust
// crates/proxy/src/gateway.rs — addition to Gateway struct

impl Gateway {
    /// Check if cross-namespace access is allowed.
    /// Default: deny all cross-namespace calls.
    pub fn is_cross_namespace_allowed(&self, source_ns: &str, target_ns: &str) -> bool {
        // Check allowlist if configured
        if let Some(ref allowlist) = self.cross_namespace_allowlist {
            if allowlist.get(&(source_ns.to_string(), target_ns.to_string())).copied() == Some(true) {
                return true;
            }
        }
        false
    }
}
```

---

## WASI-Layer Header Injection (Future Phase)

### The Current Gap

The `socket_addr_check` callback only returns `bool` (allow/deny). It cannot
inject data into the TCP stream. This means the gateway must resolve identity
via the connection table (source-port lookup) rather than from headers.

### The Future Solution: TCP Stream Wrapping

When wasmtime-wasi supports custom TCP stream wrappers, the host can intercept
the first `write()` to a socket connected to the gateway and inject identity
headers before the app's data:

```rust
// Future: when wasmtime-wasi provides a TCP stream wrapper hook

builder.tcp_stream_wrapper(move |stream, peer_addr| {
    if peer_addr.port() == common::INTERNAL_GATEWAY_PORT {
        // Wrap the stream to inject headers on first write
        IdentityInjectingStream::new(stream, namespace.clone(), app_id.clone(), tid)
    } else {
        stream
    }
});

struct IdentityInjectingStream {
    inner: TcpStream,
    headers_injected: bool,
    namespace: String,
    app_id: String,
    tid: u32,
}

impl IdentityInjectingStream {
    async fn write(&mut self, buf: &[u8]) -> Result<usize> {
        if !self.headers_injected {
            // Inject identity headers before the app's data
            let header_block = format!(
                "X-Namespace: {}\r\nX-Source-App: {}\r\nX-Source-Tid: {}\r\n",
                self.namespace, self.app_id, self.tid
            );
            self.inner.write_all(header_block.as_bytes()).await?;
            self.headers_injected = true;
        }
        self.inner.write_all(buf).await
    }
}
```

This would allow the gateway to read identity from headers (simpler, more
explicit) while still using the connection table as a verification layer.

**Status:** This requires wasmtime-wasi changes. The connection-table approach
works today without any wasmtime-wasi modifications.

---

## Security Considerations

### 1. TID Reuse

**Problem:** When a Wasm instance exits, its thread returns to Tokio's pool.
A new instance on the same thread gets the same TID.

**Mitigations (defense in depth):**

1. **Immediate deregistration:** The Supervisor deregisters the TID inside
   the `spawn_blocking` closure after `instance.run()` returns.
2. **Connection table cleanup:** When eBPF detects `TCP_CLOSE` on the
   gateway connection, the entry is removed from the connection table.
3. **Periodic cleanup:** The health loop removes stale TIDs where the
   thread no longer exists (`kill(tid, 0)` returns `ESRCH`).
4. **Timestamp validation:** The `registered_at_ns` field allows the
   gateway to detect suspiciously old registrations.

### 2. Can the App Bypass the Connection Table?

**No, if eBPF is active:**
- eBPF tracks TCP connections at the kernel level
- The connection table is populated by kernel events, not by the app
- The app cannot forge which source port its connection uses

**Yes, if eBPF is not active:**
- The gateway falls back to `NamespaceRegistry.port_to_app`
- This is TOCTOU-vulnerable (ports are reused)
- The `socket_addr_check` provides some protection (blocks unknown ports)

### 3. Can the App Forge Identity Headers?

**Not useful, because the gateway strips all identity headers before
processing.** The gateway resolves identity from the connection table,
not from headers. Any headers the app sends are removed.

### 4. eBPF Map Access Control

eBPF maps are accessible via `bpf()` syscall to any process with `CAP_BPF`
or `CAP_SYS_ADMIN`.

**Mitigation:**
- The `wasm-node` process should be the only process with `CAP_BPF`
- Wasm instances cannot call `bpf()` (WASI doesn't expose it)
- Use `BPF_F_RDONLY_PROG` for maps that eBPF programs only read

### 5. Unregistered TID Connecting to Gateway

If an unregistered TID connects to port 9080, eBPF emits an
`UnregisteredTid` event. The gateway denies the request (`resolve_identity`
returns `None`).

This could happen if:
- The TID registration failed (map full)
- A non-Wasm thread in the process connects to the gateway
- A Wasm module bypassed the WASI layer (should not be possible)

### 6. Stale port_to_tid Entries

If the eBPF consumer falls behind, `port_to_tid` may miss disconnection
events, leaving stale entries. A subsequent request on a reused source port
could be attributed to the wrong TID.

**Mitigation:**
- `deregister_tid()` also removes all `port_to_tid` entries for that TID
- The `cleanup_stale_tids()` health loop removes entries for dead TIDs
- `resolve_identity()` can validate that the TID is still alive before
  returning (optional, adds overhead)
- eBPF `TCP_CLOSE` events trigger `unbind_port()` to remove stale entries

---

## Implementation Phases

### Phase 1: Foundation (Week 1-2)

**Goal:** TID registration infrastructure + identity resolution API.

| # | Task | Crate | Files |
|---|------|-------|-------|
| 1 | Define `TidIdentity`, `NsEnforceConfig`, `NamespaceAuditEvent` structs | `ebpf-monitor` | `src/common.rs`, `bpf/src/common.rs` |
| 2 | Add `EventType` variants: `TidConnection`, `TidDisconnection`, `NamespaceAudit`, `NamespaceForgedHeader` | `ebpf-monitor` | `src/common.rs` |
| 3 | Add `MonitorEvent` variants for namespace events | `ebpf-monitor` | `src/actions.rs` |
| 4 | Add `RecoveryAction::NamespaceSecurityIncident` | `ebpf-monitor` | `src/actions.rs` |
| 5 | Create `namespace_map.rs` with `register_tid`/`deregister_tid`/`bind_port`/`unbind_port`/`resolve_identity`/`cleanup_stale_tids` | `ebpf-monitor` | `src/namespace_map.rs` |
| 6 | Add `MONITORED_TIDS` and `NS_ENFORCE_CONFIG` maps to eBPF program | `ebpf-monitor` | `bpf/src/namespace_enforcer.rs` |
| 7 | Add `ns_inet_sock_set_state` tracepoint program | `ebpf-monitor` | `bpf/src/namespace_enforcer.rs` |
| 8 | Add `ns_audit_sendto` tracepoint program | `ebpf-monitor` | `bpf/src/namespace_enforcer.rs` |
| 9 | Add `namespace_enforcer` binary target to BPF Cargo.toml | `ebpf-monitor` | `bpf/Cargo.toml` |
| 10 | Extend `loader.rs` to attach namespace enforcer programs | `ebpf-monitor` | `src/loader.rs` |
| 11 | Extend `consumer.rs` to parse `NamespaceAuditEvent` and call `bind_port`/`unbind_port` | `ebpf-monitor` | `src/consumer.rs` |
| 12 | Extend `ActionDispatcher::dispatch()` for namespace events | `ebpf-monitor` | `src/actions.rs` |
| 13 | Add `NamespaceMap` to `MonitorHandle` | `ebpf-monitor` | `src/lib.rs` |
| 14 | Add `tid` field to `ManagedInstance` | `supervisor` | `src/instance.rs` |
| 15 | Add `gettid()` helper function | `supervisor` | `src/lib.rs` |
| 16 | Modify `spawn()` to register/deregister TID, send TID via channel | `supervisor` | `src/lib.rs` |
| 17 | Modify `kill_instance_internal()` to deregister TID | `supervisor` | `src/lib.rs` |
| 18 | Add stale TID cleanup to health loop | `supervisor` | `src/lib.rs` |
| 19 | Unit tests: TidIdentity serialization, register/deregister, resolve_identity | `ebpf-monitor` | `src/namespace_map.rs` |
| 20 | Unit test: resolve_identity returns None for unknown port | `ebpf-monitor` | `src/namespace_map.rs` |

### Phase 2: Gateway Integration (Week 2-3)

**Goal:** Gateway queries NamespaceMap.resolve_identity(), enforces namespace policy.

| # | Task | Crate | Files |
|---|------|-------|-------|
| 1 | Add `namespace_map` and `ebpf_active` to `InternalGateway` | `internal_gateway` | `src/lib.rs` |
| 2 | Implement header stripping in `proxy_handler` | `internal_gateway` | `src/lib.rs` |
| 3 | Implement identity resolution via `namespace_map.resolve_identity(port)` | `internal_gateway` | `src/lib.rs` |
| 4 | Implement cross-namespace deny-by-default | `internal_gateway` | `src/lib.rs` |
| 5 | Add `allow_anonymous_internal` config flag | `internal_gateway` | `src/lib.rs` |
| 6 | Add cross-namespace allowlist to Gateway config | `proxy` | `src/gateway.rs` |
| 7 | Implement `is_cross_namespace_allowed()` | `proxy` | `src/gateway.rs` |
| 8 | Wire `NamespaceMap` from `MonitorHandle` to `InternalGateway` | `node` | `src/main.rs` |
| 9 | Integration test: same-namespace routing succeeds | `e2e` | `tests/` |
| 10 | Integration test: cross-namespace routing denied | `e2e` | `tests/` |
| 11 | Integration test: unregistered connection denied | `e2e` | `tests/` |
| 12 | Integration test: forged headers stripped and ignored | `e2e` | `tests/` |

### Phase 3: eBPF SK_MSG Enforcement (Week 4-5, Linux 5.8+)

**Goal:** SK_MSG program drops unauthorized traffic at the kernel level.

| # | Task | Crate | Files |
|---|------|-------|-------|
| 1 | Add `sockops` eBPF program (`BPF_PROG_TYPE_SOCK_OPS`) | `ebpf-monitor` | `bpf/src/namespace_enforcer.rs` |
| 2 | Add `sk_msg` eBPF program (`BPF_PROG_TYPE_SK_MSG`) | `ebpf-monitor` | `bpf/src/namespace_enforcer.rs` |
| 3 | Add `MONITORED_SOCKETS` SockHash map | `ebpf-monitor` | `bpf/src/namespace_enforcer.rs` |
| 4 | SK_MSG program: drop packets from unregistered TIDs to gateway port | `ebpf-monitor` | `bpf/src/namespace_enforcer.rs` |
| 5 | Extend loader to attach sockops + sk_msg | `ebpf-monitor` | `src/loader.rs` |
| 6 | Kernel capability detection at startup | `ebpf-monitor` | `src/loader.rs` |
| 7 | Auto-select enforcement level based on kernel capabilities | `supervisor` | `src/lib.rs` |
| 8 | Integration test: SK_MSG drops unregistered TID traffic | `e2e` | `tests/` |

**Note:** SK_MSG provides **enforcement** (drop unauthorized traffic), not
header injection. It cannot prepend data to a message. The connection-table
approach is the primary identity resolution mechanism regardless of tier.

### Phase 4: Security Hardening (Week 5-6)

**Goal:** Production-ready security.

| # | Task | Crate | Files |
|---|------|-------|-------|
| 1 | Forged header detection in eBPF audit program | `ebpf-monitor` | `bpf/src/namespace_enforcer.rs` |
| 2 | Security incident → Supervisor kills instance | `ebpf-monitor`, `supervisor` | `src/actions.rs`, `src/lib.rs` |
| 3 | Connection table TTL and expiration | `ebpf-monitor` | `src/connection_table.rs` |
| 4 | Audit logging: every internal call logged with caller, target, latency | `internal_gateway` | `src/lib.rs` |
| 5 | Cross-namespace allowlist configuration API | `proxy`, `ctl` | `src/gateway.rs`, `src/cmds/` |
| 6 | Rate limiting per source app in gateway | `internal_gateway` | `src/lib.rs` |
| 7 | eBPF map access control (BPF_F_RDONLY_PROG) | `ebpf-monitor` | `src/loader.rs` |
| 8 | Chaos test: eBPF program unload → graceful fallback | `e2e` | `tests/` |
| 9 | Performance benchmark: connection table lookup < 1μs | `e2e` | `tests/` |
| 10 | High connection count: 1000 concurrent internal requests | `e2e` | `tests/` |

---

## Testing Strategy

### Unit Tests

| Test | Crate | What It Verifies |
|------|-------|-----------------|
| `TidIdentity` round-trip | `ebpf-monitor` | `#[repr(C)]` serialization matches between Rust and C layouts |
| `register_tid` / `deregister_tid` | `ebpf-monitor` | Map operations work, lookup returns correct identity |
| `NamespaceAuditEvent` parse | `ebpf-monitor` | Ring buffer consumer correctly deserializes audit events |
| `MonitorEvent::TidConnection` dispatch | `ebpf-monitor` | Action dispatcher handles connection events correctly |
| `NsEnforceConfig` serialization | `ebpf-monitor` | Config map round-trips correctly |
| `ConnectionTable::add_connection` | `ebpf-monitor` | Source port → identity mapping works |
| `ConnectionTable::lookup` | `ebpf-monitor` | Lookup returns correct identity for registered port |
| `ConnectionTable::remove_connection` | `ebpf-monitor` | Entry removed, subsequent lookup returns None |
| `parse_internal_host()` | `internal_gateway` | Parses `app.namespace.internal` format correctly |
| `evaluate_trust()` | `internal_gateway` | Returns correct trust level for each condition |
| `is_cross_namespace_allowed()` | `proxy` | Allowlist works correctly |

### Integration Tests

| Test | What It Verifies |
|------|-----------------|
| Same-namespace routing | App A (`ns=prod`) calls App B (`ns=prod`) → succeeds |
| Cross-namespace blocking | App A (`ns=prod`) calls App C (`ns=staging`) → 403 |
| Cross-namespace allowlist | App A (`ns=prod`) calls App C (`ns=staging`) with allowlist → succeeds |
| Header forgery detection | App sends fake `X-Namespace` → header stripped, identity from resolve_identity() |
| Unregistered connection deny | Connection from unregistered port → 401 |
| TID cleanup | Kill instance → TID removed from map → new instance gets new identity |
| Rate limiting | App A exceeds RPS → 429 from gateway |
| Circuit breaker | App B fails repeatedly → gateway returns 503 |
| Anonymous deny | Request without identity → 401 |

### Chaos Tests

| Test | What It Verifies |
|------|-----------------|
| eBPF program unload | Simulate eBPF program being unloaded → graceful fallback to port_to_app |
| High connection count | 1000 concurrent internal requests → measure latency, no corruption |
| TID reuse | Kill and respawn instances rapidly → verify no identity confusion |
| Map exhaustion | Register 4096 TIDs → next registration fails gracefully |
| Gateway restart | Kill and restart gateway → NamespaceMap still valid (same process) |

---

## Fallback Behavior Matrix

| Kernel Version | eBPF Capabilities | Identity Resolution | Enforcement |
|---------------|-------------------|---------------------|-------------|
| Linux 5.8+ | BTF + SK_MSG + tracepoints | `NamespaceMap.resolve_identity()` (eBPF populates port_to_tid) | SK_MSG drops unregistered TIDs |
| Linux 5.4-5.7 | Tracepoints only | `NamespaceMap.resolve_identity()` (eBPF populates port_to_tid) | Audit only (no kernel enforcement) |
| Linux < 5.4 | None | `NamespaceRegistry.port_to_app` (TOCTOU-vulnerable) | `socket_addr_check` only |
| Windows / macOS | None | `NamespaceRegistry.port_to_app` (TOCTOU-vulnerable) | `socket_addr_check` only |

**Key insight:** The gateway always calls `NamespaceMap.resolve_identity()`.
On Linux with eBPF, the `port_to_tid` map is populated by eBPF connection
events. On non-Linux or old kernels, the gateway falls back to the existing
`port_to_app` mapping. SK_MSG provides defense-in-depth enforcement on
Linux 5.8+. The gateway's query is always synchronous and in-process.

---

## Completion Checklist

### Phase 1: Foundation
- [ ] `TidIdentity` struct defined in `ebpf-monitor/src/common.rs` and `bpf/src/common.rs`
- [ ] `NsEnforceConfig` struct defined in both common.rs files
- [ ] `NamespaceAuditEvent` struct defined in both common.rs files
- [ ] `EventType` enum extended with `TidConnection`, `TidDisconnection`, `NamespaceAudit`, `NamespaceForgedHeader`
- [ ] `MonitorEvent` enum extended with namespace events
- [ ] `RecoveryAction` enum extended with `NamespaceSecurityIncident`
- [ ] `namespace_map.rs` module created with `register_tid` / `deregister_tid` / `bind_port` / `unbind_port` / `resolve_identity` / `cleanup_stale_tids`
- [ ] `MONITORED_TIDS` eBPF hash map defined in `namespace_enforcer.rs`
- [ ] `NS_ENFORCE_CONFIG` eBPF array map defined in `namespace_enforcer.rs`
- [ ] `ns_inet_sock_set_state` tracepoint program implemented
- [ ] `ns_audit_sendto` tracepoint program implemented
- [ ] `namespace_enforcer` binary target added to `bpf/Cargo.toml`
- [ ] `loader.rs` extended to attach namespace enforcer programs
- [ ] `consumer.rs` extended to parse `NamespaceAuditEvent` and call `bind_port` / `unbind_port`
- [ ] `ActionDispatcher::dispatch()` handles namespace events
- [ ] `MonitorHandle` includes `NamespaceMap`
- [ ] `ManagedInstance` has `tid: Option<u32>` field
- [ ] `gettid()` helper function implemented
- [ ] `spawn()` calls `register_tid` after instance starts, sends TID via channel
- [ ] `spawn()` calls `deregister_tid` after instance exits
- [ ] `kill_instance_internal()` calls `deregister_tid`
- [ ] Stale TID cleanup runs in health loop
- [ ] Unit tests pass for all new code

### Phase 2: Gateway
- [ ] `InternalGateway` has `namespace_map` and `ebpf_active` fields
- [ ] Gateway strips pre-existing identity headers
- [ ] Gateway resolves identity via `namespace_map.resolve_identity(source_port)`
- [ ] Gateway denies cross-namespace calls by default
- [ ] Cross-namespace allowlist in Gateway config
- [ ] `allow_anonymous_internal` config flag
- [ ] Gateway falls back to `port_to_app` when eBPF is inactive
- [ ] Integration tests pass for same-namespace and cross-namespace

### Phase 3: SK_MSG Enforcement
- [ ] `sockops` eBPF program implemented
- [ ] `sk_msg` eBPF program implemented
- [ ] `MONITORED_SOCKETS` SockHash map defined
- [ ] SK_MSG drops packets from unregistered TIDs to gateway port
- [ ] Loader attaches sockops + sk_msg programs
- [ ] Kernel capability detection at startup
- [ ] Integration tests pass for SK_MSG enforcement

### Phase 4: Security Hardening
- [ ] Forged header detection in eBPF audit program
- [ ] Security incident → Supervisor kills instance
- [ ] Connection table TTL and expiration
- [ ] Audit logging for all internal calls
- [ ] Cross-namespace allowlist configuration API
- [ ] Rate limiting per source app in gateway
- [ ] eBPF map access control (BPF_F_RDONLY_PROG)
- [ ] Chaos tests pass
- [ ] Performance benchmark: connection table lookup < 1μs

---

## Summary

| Question | Answer |
|----------|--------|
| **Does this work?** | Yes. TID-based identity works because each `spawn_blocking` task gets its own OS thread. The gateway calls `resolve_identity(source_port)` — a synchronous in-process lookup. |
| **What's the catch?** | The `port_to_tid` map needs to be populated somehow. On Linux with eBPF, the kernel detects TCP connections and the consumer calls `bind_port()`. Without eBPF, the gateway falls back to the existing TOCTOU-vulnerable `port_to_app` mapping. |
| **Is it tamper-proof?** | For identity: yes — the `tid_to_identity` map is populated by the Supervisor at spawn time, not by the app. For enforcement: SK_MSG drops unauthorized traffic at kernel level (Linux 5.8+). The gateway strips all identity headers to prevent forgery. |
| **What's the simplest MVP?** | Phase 1 + 2: TID registration + `resolve_identity()` API + gateway namespace enforcement. Works on Linux 5.4+ (eBPF populates port_to_tid). No wasmtime-wasi changes needed. |
| **What's the endgame?** | Phase 1-4: SK_MSG enforcement on Linux 5.8+ (drop unauthorized traffic at kernel level) + synchronous identity resolution + gateway namespace enforcement. Defense in depth at every layer. |
| **Why not inject headers?** | eBPF tracepoints are read-only. SK_MSG cannot prepend data. The `resolve_identity()` approach works today without any wasmtime-wasi modifications. When wasmtime-wasi supports TCP stream wrapping, we can add header injection as an additional layer. |

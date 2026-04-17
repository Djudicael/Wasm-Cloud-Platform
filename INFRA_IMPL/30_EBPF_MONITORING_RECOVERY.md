# Step 30 — eBPF Kernel-Level Monitoring & Automated Recovery

## Goal
Implement kernel-level observability and automated recovery using eBPF programs written in
Rust. The system must:
- Monitor node health from inside the Linux kernel (process lifecycle, memory pressure,
  network anomalies, file descriptor exhaustion, disk I/O saturation)
- Detect failure conditions faster than userspace polling (sub-millisecond vs 5-second
  health loop)
- Trigger automated recovery actions (instance pruning, backpressure signaling, degraded
  mode entry) before failures cascade into user-visible outages
- Enforce resource limits at the kernel level (per-tenant connection caps, syscall policy)
- Feed kernel-level metrics into the existing Prometheus pipeline
- Require no external agents — eBPF programs are loaded by `wasm-node` itself
- Operate safely: all eBPF programs are verified by the kernel verifier and cannot crash
  the host

---

## Context & Rationale

### The Problem This Solves

Step 27 (Disaster Recovery) defines manual and CLI-driven recovery for L3–L6 failures.
Step 07 (Supervisor) uses a 5-second health loop to detect dead instances. Step 11
(Metrics) aggregates operational data at 1-minute granularity. Step 24 (Rate Limiting)
enforces per-tenant and per-IP limits in userspace.

These mechanisms share a common limitation: **they operate in userspace, on polling
intervals, after damage has already occurred**.

```
Current Detection Latency:

Failure                    │ Detection Method        │ Latency
───────────────────────────┼─────────────────────────┼───────────────
Instance crash (OOM/trap)  │ Health loop TCP probe    │ 0–5 seconds
NATS disconnection         │ NatsHealthWatcher        │ 5–30 seconds
Memory pressure            │ Prometheus alert         │ 1–5 minutes
Connection storm           │ Rate limiter (per-req)   │ After N requests
Disk filling up            │ GC + Prometheus alert    │ 5–10 minutes
FD exhaustion              │ Failed accept() call     │ At failure time
Rogue syscall from Wasm   │ None (Wasm SFI only)     │ Never detected
```

eBPF moves detection into the kernel, where events happen. Instead of polling every 5
seconds, the kernel calls our handler on every relevant event. Instead of detecting OOM
after the process is killed, we detect memory pressure before the OOM killer fires.

### Why eBPF (Not Kernel Modules, Not External Agents)

| Option          │ Safety    │ Deployment    │ Overhead    │ Kernel Access
|─────────────────┼───────────┼───────────────┼─────────────┼──────────────
| Kernel module   │ Unsafe    │ Requires reboot│ Low         │ Full
| External agent  │ Safe      │ Separate process│ High (IPC)  │ None (reads /proc)
| eBPF            │ Verified  │ In-process     │ Very low    │ Kernel hooks

eBPF programs are **verified by the Linux kernel** before loading. A program that could
hang, access out-of-bounds memory, or modify kernel state is rejected at load time. This
makes eBPF safe to ship in a production binary — a bug in the eBPF program cannot crash
the kernel.

External agents (Datadog, Falco, Tetragon) require separate deployment, configuration,
and network communication. For a shared-nothing platform where each node is self-sufficient,
adding an external dependency contradicts the architecture. eBPF programs load from within
`wasm-node` itself — no extra processes, no extra deployment.

### Why Rust (Not C) for eBPF

Traditional eBPF programs are written in C with `bpf-helpers(7)`. The aya framework
enables writing eBPF programs in Rust:

- **Memory safety**: Rust's ownership model prevents the buffer overflows and use-after-free
  bugs that plague C eBPF programs. The kernel verifier catches many of these, but Rust
  prevents them at compile time — faster development cycle.
- **Shared types**: The same Rust struct definitions are used in both the eBPF program
  (kernel side) and the userspace loader. No hand-synchronized C header ↔ Rust struct
  mapping. `aya::include_bytes_aligned!` compiles the eBPF program and embeds it in the
  binary.
- **Ecosystem**: aya provides `aya-log` for tracing from eBPF, `aya-obj` for object
  parsing, and `aya-ebpf-cty` for C-compatible types. No libbpf dependency.
- **Consistency**: The entire platform is Rust. eBPF in Rust keeps the codebase uniform.

### Why aya (Not libbpf-rs, Not RedBPF)

| Framework   │ Language │ Build System     │ Community  │ BTF Support
|─────────────┼──────────┼───────────────────┼────────────┼────────────
| libbpf-rs   │ C bindings│ Requires libbpf  │ Large      │ Yes
| RedBPF      │ Rust     │ Cargo + bpfel    │ Unmaintained│ Partial
| aya         │ Rust     │ Cargo only       │ Active     │ Yes

aya is the only actively maintained, pure-Rust eBPF framework. It requires no C toolchain
for the userspace side (the eBPF side uses `rustc --target=bpfel-unknown-none`). This
integrates cleanly with the existing Cargo workspace.

### What eBPF Can and Cannot Do

**Can do (kernel hooks available):**
- Trace process exec/exit/fork via `tracepoint/sched/sched_process_*`
- Monitor TCP connection open/close via `kprobe/tcp_v4_connect`, `tracepoint/sock/inet_sock_set_state`
- Track file descriptor allocation via `kprobe/__fd_install`, tracepoint on `do_filp_open`
- Observe memory pressure via `kprobe/try_to_free_pages`, cgroup stats
- Measure disk I/O via `tracepoint/block/block_rq_issue`
- Count syscalls per PID via `tracepoint/raw_syscalls/sys_enter`
- Detect TCP retransmits and zero-window probes via `tracepoint/sock/inet_sock_set_state`
- Enforce cgroup limits via BPF LSM hooks

**Cannot do (kernel limitations):**
- Modify userspace memory directly (must use ring buffer or BPF maps)
- Block or filter network packets (that's XDP/TC — separate concern, not needed here)
- Access arbitrary kernel memory (verifier enforces bounds)
- Run indefinitely (eBPF programs must terminate — no loops without bounded iteration)
- Work on non-Linux platforms (eBPF is Linux-only; on macOS/Windows, the monitor degrades
  gracefully to userspace fallback)

### Graceful Degradation on Non-Linux Hosts

The eBPF monitor is an **optional enhancement**. On systems without eBPF support
(Windows, macOS, older kernels < 4.15), `wasm-node` starts normally without eBPF and
falls back to the existing userspace health loop and metrics. The eBPF crate is
feature-gated:

```toml
# crates/ebpf-monitor/Cargo.toml
[features]
default = []
ebpf = ["aya", "aya-log"]  # Only compiled on Linux with kernel headers
```

No platform functionality depends on eBPF. It makes existing recovery faster and more
precise, but the system works without it.

### Failure Classification: What eBPF Automates

Extending the severity model from Step 27:

```
Severity │ Description                     │ Current Detection    │ eBPF Enhancement
─────────┼─────────────────────────────────┼──────────────────────┼──────────────────────────
   L1    │ Instance crash (OOM, trap)      │ 5s health loop       │ <1ms via tracepoint
   L2    │ Node process restart            │ systemd + redb       │ Instant via sched_process
   L3    │ Redb corruption                 │ Startup check        │ Disk I/O anomaly detection
   L4    │ Node total loss                 │ NATS heartbeat       │ Process exit tracepoint
   L5    │ Network partition               │ 5–30s NATS watcher   │ TCP retransmit detection
  NEW    │ Memory pressure (pre-OOM)        │ Prometheus alert     │ Kernel memory reclaim events
  NEW    │ FD exhaustion (approaching)      │ Failed accept()      │ fd allocation counter
  NEW    │ Connection storm (kernel-level)  │ Rate limiter         │ TCP connect tracepoint
  NEW    │ Disk I/O saturation             │ Prometheus alert     │ Block rq latency tracking
  NEW    │ Syscall anomaly from Wasm       │ None                 │ raw_syscalls/sys_enter
```

---

---

## 1. Crate Structure & Build System

### Workspace Layout

```
crates/
├── ebpf-monitor/           # Userspace loader + ring buffer consumer
│   ├── Cargo.toml
│   ├── src/
│   │   ├── lib.rs          # Public API: EbpfMonitor, MonitorConfig
│   │   ├── loader.rs       # Load eBPF programs, attach to tracepoints
│   │   ├── consumer.rs     # Ring buffer consumer, event dispatch
│   │   ├── actions.rs      # Recovery action executor
│   │   ├── fallback.rs     # Userspace fallback for non-Linux
│   │   ├── metrics.rs      # Prometheus metrics from eBPF events
│   │   └── config.rs       # MonitorConfig with thresholds
│   └── bpf/                # eBPF programs (compiled to BPF bytecode)
│       ├── Cargo.toml      # [target.bpfel-unknown-none]
│       ├── src/
│       │   ├── process_tracker.rs   # sched_process_exec/exit
│       │   ├── tcp_monitor.rs        # tcp_v4_connect, inet_sock_set_state
│       │   ├── fd_watcher.rs         # __fd_install, do_filp_open
│       │   ├── mem_pressure.rs       # Memory reclaim tracepoints
│       │   ├── disk_monitor.rs      # block_rq_issue/complete
│       │   ├── syscall_counter.rs   # raw_syscalls/sys_enter
│       │   └── common.rs            # Shared structs, map definitions
│       └── .cargo/
│           └── config.toml  # rustflags for BPF target
```

### Cargo.toml — Userspace Side

```toml
# crates/ebpf-monitor/Cargo.toml
[package]
name = "ebpf-monitor"
version = "0.1.0"
edition = "2021"

[features]
default = []
ebpf = ["aya", "aya-log"]

[dependencies]
aya = { version = "0.12", optional = true }
aya-log = { version = "0.2", optional = true }
common = { path = "../common" }
metrics = { path = "../metrics" }
prometheus = { version = "0.13", features = ["process"] }
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
libc = "0.2"
```

### Cargo.toml — eBPF Side

```toml
# crates/ebpf-monitor/bpf/Cargo.toml
[package]
name = "ebpf-monitor-bpf"
version = "0.1.0"
edition = "2021"

[dependencies]
aya-ebpf = "0.1"
aya-log-ebpf = "0.1"

[[bin]]
name = "process_tracker"
path = "src/process_tracker.rs"

[[bin]]
name = "tcp_monitor"
path = "src/tcp_monitor.rs"

[[bin]]
name = "fd_watcher"
path = "src/fd_watcher.rs"

[[bin]]
name = "mem_pressure"
path = "src/mem_pressure.rs"

[[bin]]
name = "disk_monitor"
path = "src/disk_monitor.rs"

[[bin]]
name = "syscall_counter"
path = "src/syscall_counter.rs"
```

### Build Configuration for BPF Target

```toml
# crates/ebpf-monitor/bpf/.cargo/config.toml
[target.bpfel-unknown-none]
rustflags = [
    "-C", "panic=abort",
    "-C", "overflow-checks=false",
    "-C", "link-arg=--no-threads",
]
```

### Workspace Cargo.toml Addition

```toml
# Root Cargo.toml — add to [workspace.members]
members = [
    # ... existing members ...
    "crates/ebpf-monitor",
]

# Add to [workspace.dependencies]
aya = "0.12"
aya-log = "0.2"
```

### Build Commands

```bash
# Build eBPF programs (requires rustc bpfel target)
cargo build --manifest-path crates/ebpf-monitor/bpf/Cargo.toml --target bpfel-unknown-none --release

# Build userspace loader (includes compiled eBPF bytecode via include_bytes_aligned)
cargo build --bin wasm-node --features ebpf

# Build without eBPF (fallback mode)
cargo build --bin wasm-node
```

---

## 2. Shared Data Structures (Kernel ↔ Userspace)

All structs shared between eBPF programs and userspace must be `#[repr(C)]` with
C-compatible types (`u32`, `u64`, fixed-size arrays — no `String`, no `Vec`).

```rust
// crates/ebpf-monitor/bpf/src/common.rs
// Also accessible from userspace via aya include

/// Maximum length for comm (process name) in kernel — 16 bytes including null.
pub const TASK_COMM_LEN: usize = 16;

/// Maximum length for IP address as u8 array (IPv6 = 16 bytes).
pub const IP_ADDR_LEN: usize = 16;

/// Event types sent from eBPF to userspace via ring buffer.
#[repr(u32)]
pub enum EventType {
    /// A process was exec'd. Check if it's a wasm-node child.
    ProcessExec = 1,
    /// A process exited. If it's a Wasm instance, notify supervisor.
    ProcessExit = 2,
    /// A TCP connection was opened.
    TcpConnect = 3,
    /// A TCP connection was closed.
    TcpClose = 4,
    /// TCP retransmit detected (early partition warning).
    TcpRetransmit = 5,
    /// File descriptor opened.
    FdOpen = 6,
    /// Memory pressure event (kernel reclaim triggered).
    MemPressure = 7,
    /// Disk I/O latency exceeded threshold.
    DiskSlowIo = 8,
    /// Syscall from monitored PID in unexpected category.
    SyscallAnomaly = 9,
    /// FD count for a PID exceeded soft limit.
    FdLimitApproaching = 10,
}

/// Header for every event sent through the ring buffer.
#[repr(C)]
pub struct EventHeader {
    pub event_type: u32,
    pub timestamp_ns: u64,  // ktime (CLOCK_MONOTONIC)
    pub pid: u32,
    pub tid: u32,
}

/// Process exec/exit event.
#[repr(C)]
pub struct ProcessEvent {
    pub header: EventHeader,
    pub comm: [u8; TASK_COMM_LEN],
    pub exit_code: u32,   // 0 for exec events
    pub signal: u32,      // 0 for exec events; signal number for exit
    pub ppid: u32,        // Parent PID (to identify wasm-node children)
    pub cgroup_id: u64,   // cgroup v2 ID for tenant attribution
}

/// TCP connection event.
#[repr(C)]
pub struct TcpEvent {
    pub header: EventHeader,
    pub src_addr: [u8; IP_ADDR_LEN],
    pub src_port: u16,
    pub dst_addr: [u8; IP_ADDR_LEN],
    pub dst_port: u16,
    pub old_state: u32,    // TCP FSM old state
    pub new_state: u32,    // TCP FSM new state
    pub retransmits: u32,  // Cumulative retransmit count at event time
    pub rtt_us: u64,       // Smoothed RTT in microseconds
}

/// File descriptor event.
#[repr(C)]
pub struct FdEvent {
    pub header: EventHeader,
    pub fd: u32,
    pub fd_type: u32,      // Enum: FdType { File, Socket, Pipe, Other }
    pub current_fd_count: u32,  // Total open FDs for this PID
    pub fd_soft_limit: u32,     // Configured soft limit
}

/// Memory pressure event.
#[repr(C)]
pub struct MemPressureEvent {
    pub header: EventHeader,
    pub free_pages: u64,
    pub reclaim_pages: u64,
    pub pressure_level: u32,  // 0=low, 1=medium, 2=critical
    pub anon_pages: u64,      // Anonymous (Wasm linear memory) pages
}

/// Disk I/O event.
#[repr(C)]
pub struct DiskIoEvent {
    pub header: EventHeader,
    pub dev_major: u32,
    pub dev_minor: u32,
    pub sector: u64,
    pub nr_sector: u32,
    pub latency_ns: u64,     // Time from submit to complete
    pub io_type: u32,         // 0=read, 1=write, 2=sync
}

/// Syscall anomaly event.
#[repr(C)]
pub struct SyscallEvent {
    pub header: EventHeader,
    pub syscall_nr: u64,
    pub syscall_category: u32,  // Enum: SyscallCategory
    pub count_in_window: u64,   // Count in the last sampling window
}

/// Syscall categories for policy enforcement.
#[repr(u32)]
pub enum SyscallCategory {
    /// Allowed: read, write, openat, close, fstat, mmap, mprotect, etc.
    Normal = 0,
    /// Suspicious: ptrace, perf_event_open, bpf, mount, umount, setuid
    PrivilegeEscalation = 1,
    /// Network control: socket, bind, listen, connect, setsockopt
    NetworkControl = 2,
    /// Process control: fork, clone, execve, kill, tgkill
    ProcessControl = 3,
}

/// Configuration map (userspace → kernel).
#[repr(C)]
pub struct MonitorConfigMap {
    /// PID of the wasm-node process (to filter relevant events).
    pub node_pid: u32,
    /// FD soft limit per Wasm instance PID.
    pub fd_soft_limit: u32,
    /// FD hard limit per Wasm instance PID (trigger kill).
    pub fd_hard_limit: u32,
    /// Memory pressure threshold (pages) for "low" alert.
    pub mem_low_threshold_pages: u64,
    /// Memory pressure threshold (pages) for "critical" alert.
    pub mem_critical_threshold_pages: u64,
    /// Disk I/O latency threshold (nanoseconds) for "slow" alert.
    pub disk_slow_threshold_ns: u64,
    /// Maximum TCP connections per PID before alert.
    pub tcp_conn_limit_per_pid: u32,
    /// Syscall rate limit (per second) for suspicious categories.
    pub syscall_rate_limit: u64,
    /// Sampling period for periodic counters (nanoseconds).
    pub sampling_period_ns: u64,
}
```

---

## 3. eBPF Program: Process Tracker

Monitors process lifecycle events for the wasm-node process and its children (Wasm
instances run on `spawn_blocking` threads — they share the same PID but we also track
the thread group IDs).

### What It Detects

- **Wasm instance thread exit**: A `spawn_blocking` thread that panics or is killed
  triggers `sched_process_exit`. The Supervisor learns about it in <1ms instead of
  waiting for the next health loop tick.
- **OOM kill**: If the Linux OOM killer selects a Wasm instance's memory cgroup, the
  exit signal is `SIGKILL` (9) and the exit code is 0. The eBPF program detects this
  and flags it as an OOM event.
- **Unexpected child process**: If a Wasm module somehow spawns a child process (should
  be impossible under WASI, but defense in depth), the `sched_process_exec` tracepoint
  fires for the new PID with `ppid == node_pid`.

### eBPF Program

```rust
// crates/ebpf-monitor/bpf/src/process_tracker.rs
#![no_std]
#![no_main]

use aya_ebpf::{
    macros::{tracepoint, map},
    maps::RingBuf,
    programs::TracePointContext,
    bindings::BPF_F_CURRENT_CPU,
    cty::c_long,
};
use common::{EventType, EventHeader, ProcessEvent, TASK_COMM_LEN, MonitorConfigMap};

#[map]
static CONFIG: aya_ebpf::maps::Array<MonitorConfigMap> =
    aya_ebpf::maps::Array::with_max_entries(1, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_max_entries(1024 * 1024, 0); // 1 MB

#[tracepoint]
pub fn sched_process_exec(ctx: TracePointContext) -> c_long {
    match try_sched_process_exec(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_sched_process_exec(ctx: TracePointContext) -> c_long, anyhow::Error> {
    let config = CONFIG.get(0).ok_or(0)?;
    let node_pid = config.node_pid;

    // Read tracepoint fields: common_pid, ppid, comm
    let pid: u32 = ctx.read(0)?;
    let ppid: u32 = ctx.read(1)?;

    // Only care about children of wasm-node
    if ppid != node_pid && pid != node_pid {
        return Ok(0);
    }

    let mut event = ProcessEvent {
        header: EventHeader {
            event_type: EventType::ProcessExec as u32,
            timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
            pid,
            tid: 0, // Not available in this tracepoint
        },
        comm: [0u8; TASK_COMM_LEN],
        exit_code: 0,
        signal: 0,
        ppid,
        cgroup_id: unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() },
    };

    // Read comm from tracepoint args
    let comm: [u8; TASK_COMM_LEN] = ctx.read(2)?;
    event.comm = comm;

    EVENTS.output(&event, 0);
    Ok(0)
}

#[tracepoint]
pub fn sched_process_exit(ctx: TracePointContext) -> c_long {
    match try_sched_process_exit(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_sched_process_exit(ctx: TracePointContext) -> c_long, anyhow::Error> {
    let config = CONFIG.get(0).ok_or(0)?;
    let node_pid = config.node_pid;

    let pid: u32 = ctx.read(0)?;
    let ppid: u32 = ctx.read(1)?;

    if ppid != node_pid && pid != node_pid {
        return Ok(0);
    }

    let exit_code: u32 = ctx.read(2)?;
    let signal: u32 = ctx.read(3)?;

    let mut event = ProcessEvent {
        header: EventHeader {
            event_type: EventType::ProcessExit as u32,
            timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
            pid,
            tid: 0,
        },
        comm: [0u8; TASK_COMM_LEN],
        exit_code,
        signal,
        ppid,
        cgroup_id: unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() },
    };

    let comm: [u8; TASK_COMM_LEN] = ctx.read(4)?;
    event.comm = comm;

    EVENTS.output(&event, 0);
    Ok(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
```

### Userspace Action on Process Exit

When the ring buffer consumer receives a `ProcessExit` event with `ppid == node_pid`:

1. **OOM kill detected** (`signal == 9`): Immediately notify the Supervisor to mark the
   instance as dead and trigger a respawn with reduced memory limits. Log a `SECURITY`
   alert. Emit `wasm_instance_oom_kill_total` Prometheus metric.
2. **Normal exit** (`signal == 0`): The instance exited cleanly. The Supervisor's health
   loop will discover this on its next tick, but the eBPF event allows **preemptive
   removal from the upstream table** — no 502 errors for requests routed to the dead
   instance during the 5-second gap.
3. **Signal death** (`signal != 0 && signal != 9`): The instance was killed by a signal
   (e.g., `SIGABRT` from a Wasm trap). Log the signal and notify the Supervisor.

---

## 4. eBPF Program: TCP Connection Monitor

Tracks TCP connection state transitions at the kernel level. This provides:

- **Connection count per PID**: Enforce per-instance connection limits at the kernel
  level, complementing the userspace rate limiter (Step 24).
- **Retransmit detection**: TCP retransmits are the earliest sign of network degradation.
  A spike in retransmits for the NATS connection predicts a partition before the
  `NatsHealthWatcher` detects it.
- **Connection storm detection**: A sudden burst of `TCP_SYN_SENT` transitions indicates
  a connection storm (either legitimate traffic spike or attack).

### eBPF Program

```rust
// crates/ebpf-monitor/bpf/src/tcp_monitor.rs
#![no_std]
#![no_main]

use aya_ebpf::{
    macros::{kprobe, tracepoint, map},
    maps::{RingBuf, HashMap, Array},
    programs::TracePointContext,
    cty::c_long,
};
use common::{
    EventType, EventHeader, TcpEvent, MonitorConfigMap,
    IP_ADDR_LEN,
};

#[map]
static CONFIG: Array<MonitorConfigMap> = Array::with_max_entries(1, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_max_entries(1024 * 1024, 0);

/// Per-PID TCP connection counter.
#[map]
static TCP_CONN_COUNT: HashMap<u32, u32> = HashMap::with_max_entries(10240, 0);

/// Per-PID retransmit counter (reset every sampling period).
#[map]
static TCP_RETRANSMIT_COUNT: HashMap<u32, u64> = HashMap::with_max_entries(10240, 0);

#[tracepoint]
pub fn inet_sock_set_state(ctx: TracePointContext) -> c_long {
    match try_inet_sock_set_state(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_inet_sock_set_state(ctx: TracePointContext) -> c_long, anyhow::Error> {
    let config = CONFIG.get(0).ok_or(0)?;

    // Read tracepoint fields
    let pid: u32 = ctx.read(0)?;  // PID of the process owning the socket
    let old_state: u32 = ctx.read(1)?;
    let new_state: u32 = ctx.read(2)?;
    let src_port: u16 = ctx.read(3)?;
    let dst_port: u16 = ctx.read(4)?;
    let retransmits: u32 = ctx.read(5)?;
    let rtt_us: u64 = ctx.read(6)?;

    // Only monitor wasm-node and its children
    if pid != config.node_pid {
        // Check if this PID is a child — we'd need a child PID set map.
        // For simplicity, we track all PIDs in the same cgroup.
    }

    // Track connection count
    if new_state == 1 { // TCP_ESTABLISHED
        if let Some(count) = TCP_CONN_COUNT.get_ptr_mut(&pid) {
            *count += 1;
        } else {
            TCP_CONN_COUNT.insert(&pid, &1, 0)?;
        }

        // Check against limit
        let current = *TCP_CONN_COUNT.get(&pid).ok_or(0)?;
        if current > config.tcp_conn_limit_per_pid {
            let event = TcpEvent {
                header: EventHeader {
                    event_type: EventType::TcpConnect as u32,
                    timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
                    pid,
                    tid: 0,
                },
                src_addr: [0u8; IP_ADDR_LEN],
                src_port,
                dst_addr: [0u8; IP_ADDR_LEN],
                dst_port,
                old_state,
                new_state,
                retransmits,
                rtt_us,
            };
            EVENTS.output(&event, BPF_F_CURRENT_CPU as u64);
        }
    } else if new_state == 7 { // TCP_CLOSE
        if let Some(count) = TCP_CONN_COUNT.get_ptr_mut(&pid) {
            if *count > 0 {
                *count -= 1;
            }
        }
    }

    // Detect retransmits (state goes to TCP_RETRANS or retransmit counter increases)
    if retransmits > 0 {
        if let Some(count) = TCP_RETRANSMIT_COUNT.get_ptr_mut(&pid) {
            *count += retransmits as u64;
        } else {
            TCP_RETRANSMIT_COUNT.insert(&pid, &(retransmits as u64), 0)?;
        }

        let total = *TCP_RETRANSMIT_COUNT.get(&pid).ok_or(0)?;
        // Alert if > 10 retransmits in current window
        if total > 10 {
            let event = TcpEvent {
                header: EventHeader {
                    event_type: EventType::TcpRetransmit as u32,
                    timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
                    pid,
                    tid: 0,
                },
                src_addr: [0u8; IP_ADDR_LEN],
                src_port,
                dst_addr: [0u8; IP_ADDR_LEN],
                dst_port,
                old_state,
                new_state,
                retransmits,
                rtt_us,
            };
            EVENTS.output(&event, BPF_F_CURRENT_CPU as u64);
        }
    }

    Ok(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
```

### Userspace Action on TCP Events

1. **Connection limit exceeded**: Signal the backpressure system (Step 24) to reject
   new connections for the affected app. The rate limiter already exists; the eBPF event
   provides a kernel-level trigger that fires before the userspace limiter even sees the
   request.
2. **Retransmit spike on NATS port (4222)**: Pre-emptively transition `NatsHealth` to
   disconnected state. Start the degraded-mode behavior (Step 27, L5) before the actual
   disconnect happens. This gives the node a 5–30 second head start on catch-up.
3. **Connection storm**: Activate the Slowloris protection timeout settings (Step 24)
   proactively, reducing `request_header_read_timeout` and `max_connections_per_ip`
   temporarily.

---

## 5. eBPF Program: File Descriptor Watcher

File descriptor exhaustion is a silent killer. When a process hits `RLIMIT_NOFILE`,
`accept()` returns `EMFILE` and the node cannot accept new connections. The existing
system detects this only when `accept()` fails — by then, the node is already refusing
traffic.

### What It Detects

- **FD count per PID**: Track the running count of open file descriptors for the
  `wasm-node` process and each Wasm instance thread.
- **Approaching limit**: When FD count exceeds 80% of the soft limit, emit a warning.
- **Leak detection**: If FD count increases monotonically over 3 sampling windows
  (30 seconds), flag a potential leak.
- **FD type breakdown**: Distinguish between file FDs, socket FDs, and pipe FDs to
  identify the source of leaks.

### eBPF Program

```rust
// crates/ebpf-monitor/bpf/src/fd_watcher.rs
#![no_std]
#![no_main]

use aya_ebpf::{
    macros::{kprobe, map},
    maps::{RingBuf, HashMap, Array},
    programs::KProbeContext,
    cty::c_long,
};
use common::{EventType, EventHeader, FdEvent, MonitorConfigMap};

#[map]
static CONFIG: Array<MonitorConfigMap> = Array::with_max_entries(1, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_max_entries(512 * 1024, 0);

/// Per-PID FD counter.
#[map]
static FD_COUNT: HashMap<u32, u32> = HashMap::with_max_entries(10240, 0);

#[kprobe]
pub fn fd_install(ctx: KProbeContext) -> c_long {
    match try_fd_install(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_fd_install(ctx: KProbeContext) -> c_long, anyhow::Error> {
    let config = CONFIG.get(0).ok_or(0)?;

    // fd_install(struct file *file, unsigned int fd)
    let pid: u32 = unsafe { aya_ebpf::helpers::bpf_get_current_pid_tgid() } as u32;
    let fd: u32 = ctx.arg(1).ok_or(0)?;

    // Increment FD count
    let new_count = if let Some(count) = FD_COUNT.get_ptr_mut(&pid) {
        *count += 1;
        *count
    } else {
        FD_COUNT.insert(&pid, &1, 0)?;
        1
    };

    // Check against soft limit
    if new_count >= config.fd_soft_limit {
        let event = FdEvent {
            header: EventHeader {
                event_type: EventType::FdLimitApproaching as u32,
                timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
                pid,
                tid: 0,
            },
            fd,
            fd_type: 0, // Would need additional logic to determine type
            current_fd_count: new_count,
            fd_soft_limit: config.fd_soft_limit,
        };
        EVENTS.output(&event, 0);
    }

    // Check against hard limit — this is critical
    if new_count >= config.fd_hard_limit {
        let event = FdEvent {
            header: EventHeader {
                event_type: EventType::FdOpen as u32, // Reuse with high count
                timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
                pid,
                tid: 0,
            },
            fd,
            fd_type: 0,
            current_fd_count: new_count,
            fd_soft_limit: config.fd_hard_limit,
        };
        EVENTS.output(&event, 0);
    }

    Ok(0)
}

#[kprobe]
pub fn do_filp_close(ctx: KProbeContext) -> c_long {
    match try_do_filp_close(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_do_filp_close(_ctx: KProbeContext) -> c_long, anyhow::Error> {
    let pid: u32 = unsafe { aya_ebpf::helpers::bpf_get_current_pid_tgid() } as u32;

    if let Some(count) = FD_COUNT.get_ptr_mut(&pid) {
        if *count > 0 {
            *count -= 1;
        }
    }

    Ok(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
```

### Userspace Action on FD Events

1. **Soft limit approaching (80%)**: Log a warning. Emit `wasm_fd_usage_ratio`
   Prometheus gauge. If the PID is a Wasm instance, notify the Supervisor to consider
   pruning idle instances to free FDs.
2. **Hard limit approaching (95%)**: This is critical. The Supervisor must immediately
   kill the most idle Wasm instance to free FDs before `accept()` fails. Activate
   backpressure to stop accepting new connections until FD count drops.
3. **FD leak detected**: If FD count increases monotonically over 3 consecutive
   10-second windows, log a `SECURITY` alert. The Supervisor kills the leaking instance
   and marks it for investigation (audit log entry).

---

## 6. eBPF Program: Memory Pressure Sentinel

The OOM killer is a last resort. By the time it fires, the node is already in trouble:
the killed process loses all in-flight requests, and the Supervisor must respawn it.
Memory pressure events from the kernel's reclaim machinery fire **before** the OOM
killer, giving the node a window to shed load proactively.

### What It Detects

- **kswapd activity**: The kernel's background reclaim thread (`kswapd`) wakes up when
  free memory drops below the high watermark. This is the earliest sign of pressure.
- **Direct reclaim**: When `try_to_free_pages` is called from the allocation path, the
  system is under significant pressure — allocation latency increases.
- **OOM notification**: The `vmpressure` notifier fires at three levels: low, medium,
  critical. At "critical", the OOM killer is about to fire.
- **Anonymous page tracking**: Wasm linear memory is anonymous (not file-backed). Tracking
  `NR_ANON_PAGES` per cgroup identifies which tenant is consuming memory.

### eBPF Program

```rust
// crates/ebpf-monitor/bpf/src/mem_pressure.rs
#![no_std]
#![no_main]

use aya_ebpf::{
    macros::{kprobe, tracepoint, map},
    maps::{RingBuf, Array, PerCpuHashMap},
    programs::{KProbeContext, TracePointContext},
    cty::c_long,
};
use common::{EventType, EventHeader, MemPressureEvent, MonitorConfigMap};

#[map]
static CONFIG: Array<MonitorConfigMap> = Array::with_max_entries(1, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_max_entries(512 * 1024, 0);

/// Last reported pressure level per cgroup (to avoid duplicate events).
#[map]
static LAST_PRESSURE: PerCpuHashMap<u64, u32> = PerCpuHashMap::with_max_entries(256, 0);

#[kprobe]
pub fn try_to_free_pages(ctx: KProbeContext) -> c_long {
    match try_try_to_free_pages(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_try_to_free_pages(ctx: KProbeContext) -> c_long, anyhow::Error> {
    let config = CONFIG.get(0).ok_or(0)?;

    // Read function arguments: gfp_mask, order
    let _gfp_mask: u32 = ctx.arg(0).ok_or(0)?;
    let order: u32 = ctx.arg(1).ok_or(0)?;

    // Direct reclaim means memory is scarce
    let cgroup_id = unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() };
    let pid: u32 = unsafe { aya_ebpf::helpers::bpf_get_current_pid_tgid() } as u32;

    // Read memory stats from /proc/meminfo equivalent
    // In eBPF, we use bpf_meminfo_type_id or read from cgroup memory.stat
    // For simplicity, we report the event and let userspace read detailed stats

    let pressure_level = if order >= 3 {
        2 // Critical: high-order allocation failing
    } else {
        1 // Medium: direct reclaim triggered
    };

    // Deduplicate: only send if pressure level changed
    let last = LAST_PRESSURE.get(&cgroup_id).copied().unwrap_or(0);
    if last >= pressure_level {
        return Ok(0);
    }
    LAST_PRESSURE.insert(&cgroup_id, &pressure_level, 0)?;

    let event = MemPressureEvent {
        header: EventHeader {
            event_type: EventType::MemPressure as u32,
            timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
            pid,
            tid: 0,
        },
        free_pages: 0,        // Userspace reads from /proc/meminfo
        reclaim_pages: 0,     // Userspace reads from /proc/meminfo
        pressure_level,
        anon_pages: 0,        // Userspace reads from cgroup memory.stat
    };

    EVENTS.output(&event, 0);
    Ok(0)
}

#[tracepoint]
pub fn vmpressure_level_change(ctx: TracePointContext) -> c_long {
    match try_vmpressure(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_vmpressure(ctx: TracePointContext) -> c_long, anyhow::Error> {
    let config = CONFIG.get(0).ok_or(0)?;
    let cgroup_id = unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() };
    let level: u32 = ctx.read(0)?; // 0=low, 1=medium, 2=critical
    let pid: u32 = unsafe { aya_ebpf::helpers::bpf_get_current_pid_tgid() } as u32;

    let last = LAST_PRESSURE.get(&cgroup_id).copied().unwrap_or(0);
    if last >= level {
        return Ok(0);
    }
    LAST_PRESSURE.insert(&cgroup_id, &level, 0)?;

    let event = MemPressureEvent {
        header: EventHeader {
            event_type: EventType::MemPressure as u32,
            timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
            pid,
            tid: 0,
        },
        free_pages: 0,
        reclaim_pages: 0,
        pressure_level: level,
        anon_pages: 0,
    };

    EVENTS.output(&event, 0);
    Ok(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
```

### Userspace Action on Memory Pressure

The response is **graduated** based on pressure level:

```
Pressure Level │ Action
───────────────┼──────────────────────────────────────────────────────────────
Low            │ Log info. Emit wasm_memory_pressure_level = 1 metric.
               │ No instance action.
───────────────┼──────────────────────────────────────────────────────────────
Medium         │ Log warning. Emit wasm_memory_pressure_level = 2 metric.
               │ Supervisor prunes all idle instances (idle_timeout = 0).
               │ Backpressure signal set to "rejecting" for 30s.
               │ No new cold starts until pressure drops.
───────────────┼──────────────────────────────────────────────────────────────
Critical       │ Log error. Emit wasm_memory_pressure_level = 3 metric.
               │ Supervisor kills the largest Wasm instance (most memory).
               │ All non-essential instances killed.
               │ Backpressure signal set to "rejecting" indefinitely.
               │ NATS event: Event::NodeUnderPressure { node_id }
               │ Other nodes stop steering traffic here.
```

This graduated response prevents the OOM killer from ever firing. The node sheds load
proactively, keeping the most critical instances alive while freeing memory.

---

## 7. eBPF Program: Disk I/O Monitor

Redb is an embedded database. Its performance depends on disk I/O latency. When the disk
is saturated (e.g., a noisy neighbor on shared hardware, or redb compaction), write
latency spikes and the node's event processing slows down.

### What It Detects

- **I/O latency per device**: Time from `block_rq_issue` to `block_rq_complete`. If
  latency exceeds the configured threshold (default: 50ms), emit a `DiskSlowIo` event.
- **Write amplification**: Track the ratio of bytes written to the device vs. bytes
  written by redb. High amplification indicates compaction or journaling overhead.
- **I/O queue depth**: If the block device queue depth exceeds a threshold, the device
  is saturated and new writes will be delayed.

### eBPF Program

```rust
// crates/ebpf-monitor/bpf/src/disk_monitor.rs
#![no_std]
#![no_main]

use aya_ebpf::{
    macros::{tracepoint, map},
    maps::{RingBuf, HashMap, Array},
    programs::TracePointContext,
    cty::c_long,
};
use common::{EventType, EventHeader, DiskIoEvent, MonitorConfigMap};

#[map]
static CONFIG: Array<MonitorConfigMap> = Array::with_max_entries(1, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_max_entries(512 * 1024, 0);

/// Track I/O start time per request (key = sector number).
#[map]
static IO_START_TIME: HashMap<u64, u64> = HashMap::with_max_entries(65536, 0);

#[tracepoint]
pub fn block_rq_issue(ctx: TracePointContext) -> c_long {
    match try_block_rq_issue(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_block_rq_issue(ctx: TracePointContext) -> c_long, anyhow::Error> {
    let dev: u32 = ctx.read(0)?;    // dev major:minor
    let sector: u64 = ctx.read(1)?; // starting sector
    let nr_sector: u32 = ctx.read(2)?; // number of sectors
    let _io_type: u32 = ctx.read(3)?; // read/write/sync

    let now = unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() };
    IO_START_TIME.insert(&sector, &now, 0)?;

    Ok(0)
}

#[tracepoint]
pub fn block_rq_complete(ctx: TracePointContext) -> c_long {
    match try_block_rq_complete(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_block_rq_complete(ctx: TracePointContext) -> c_long, anyhow::Error> {
    let config = CONFIG.get(0).ok_or(0)?;

    let dev: u32 = ctx.read(0)?;
    let sector: u64 = ctx.read(1)?;
    let nr_sector: u32 = ctx.read(2)?;
    let io_type: u32 = ctx.read(3)?;

    let now = unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() };

    if let Some(start_ns) = IO_START_TIME.get(&sector) {
        let latency_ns = now - start_ns;

        if latency_ns > config.disk_slow_threshold_ns {
            let dev_major = (dev >> 20) & 0xfff;
            let dev_minor = dev & 0xfffff;

            let event = DiskIoEvent {
                header: EventHeader {
                    event_type: EventType::DiskSlowIo as u32,
                    timestamp_ns: now,
                    pid: 0,
                    tid: 0,
                },
                dev_major,
                dev_minor,
                sector,
                nr_sector,
                latency_ns,
                io_type,
            };
            EVENTS.output(&event, 0);
        }

        // Clean up
        let _ = IO_START_TIME.remove(&sector);
    }

    Ok(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
```

### Userspace Action on Disk I/O Events

1. **Slow I/O detected**: Log warning with device, latency, and I/O type. Emit
   `wasm_disk_io_latency_seconds` Prometheus histogram. If the device is the one holding
   `state.redb`, switch redb to read-only mode temporarily (reject writes, serve reads).
2. **Sustained slow I/O (>30s)**: The node enters degraded mode. Publish
   `Event::NodeUnderPressure` to NATS. Other nodes stop steering traffic here. The node
   continues serving cached reads but defers all writes until disk recovers.
3. **I/O recovered**: Log info. Exit degraded mode. Publish `Event::NodeReady`.

---

## 8. eBPF Program: Syscall Anomaly Detector

Wasm SFI (Step 13) prevents a Wasm module from making direct syscalls — all syscalls go
through Wasmtime's WASI layer. But defense in depth means we also monitor at the kernel
level. If a Wasm module somehow bypasses Wasmtime (a hypothetical Wasmtime bug), the
syscall monitor catches it.

### What It Detects

- **Syscall rate per PID**: Count syscalls per second for each Wasm instance thread.
  An infinite loop that makes syscalls (e.g., `clock_gettime` in a tight loop) will
  show an anomalously high rate.
- **Privileged syscalls**: If a Wasm instance thread makes `ptrace`, `bpf`, `mount`,
  `setuid`, or other privilege-escalation syscalls, it's a security incident.
- **Unexpected network syscalls**: A Wasm instance that calls `bind()` on an
  unauthorized port (not its pre-bound port) is violating the network policy.

### eBPF Program

```rust
// crates/ebpf-monitor/bpf/src/syscall_counter.rs
#![no_std]
#![no_main]

use aya_ebpf::{
    macros::{tracepoint, map},
    maps::{RingBuf, HashMap, Array, PerCpuHashMap},
    programs::TracePointContext,
    cty::c_long,
};
use common::{
    EventType, EventHeader, SyscallEvent, SyscallCategory,
    MonitorConfigMap,
};

#[map]
static CONFIG: Array<MonitorConfigMap> = Array::with_max_entries(1, 0);

#[map]
static EVENTS: RingBuf = RingBuf::with_max_entries(512 * 1024, 0);

/// Per-PID syscall count in current window.
#[map]
static SYSCALL_COUNTS: PerCpuHashMap<u32, u64> = PerCpuHashMap::with_max_entries(10240, 0);

/// Per-PID suspicious syscall count in current window.
#[map]
static SUSPICIOUS_COUNTS: PerCpuHashMap<u32, u64> = PerCpuHashMap::with_max_entries(10240, 0);

/// Set of PIDs that are wasm-node children (populated by process_tracker).
#[map]
static MONITORED_PIDS: HashMap<u32, u8> = HashMap::with_max_entries(10240, 0);

/// Privileged syscall numbers (x86_64).
const SYS_PTRACE: u64 = 101;
const SYS_BPF: u64 = 321;
const SYS_MOUNT: u64 = 165;
const SYS_UMOUNT: u64 = 166;
const SYS_SETUID: u64 = 105;
const SYS_SETGID: u64 = 106;
const SYS_EXECVE: u64 = 59;

#[tracepoint]
pub fn sys_enter(ctx: TracePointContext) -> c_long {
    match try_sys_enter(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_sys_enter(ctx: TracePointContext) -> c_long, anyhow::Error> {
    let config = CONFIG.get(0).ok_or(0)?;

    let pid: u32 = unsafe { aya_ebpf::helpers::bpf_get_current_pid_tgid() } as u32;
    let syscall_nr: u64 = ctx.read(0)?;

    // Only monitor wasm-node children
    if MONITORED_PIDS.get(&pid).is_none() && pid != config.node_pid {
        return Ok(0);
    }

    // Increment total syscall count
    if let Some(count) = SYSCALL_COUNTS.get_ptr_mut(&pid) {
        *count += 1;
    } else {
        SYSCALL_COUNTS.insert(&pid, &1, 0)?;
    }

    // Check for privileged syscalls
    let category = match syscall_nr {
        SYS_PTRACE | SYS_BPF | SYS_MOUNT | SYS_UMOUNT | SYS_SETUID | SYS_SETGID => {
            SyscallCategory::PrivilegeEscalation as u32
        }
        SYS_EXECVE => SyscallCategory::ProcessControl as u32,
        _ => SyscallCategory::Normal as u32,
    };

    if category != SyscallCategory::Normal as u32 {
        if let Some(count) = SUSPICIOUS_COUNTS.get_ptr_mut(&pid) {
            *count += 1;
        } else {
            SUSPICIOUS_COUNTS.insert(&pid, &1, 0)?;
        }

        let suspicious_count = *SUSPICIOUS_COUNTS.get(&pid).ok_or(0)?;

        let event = SyscallEvent {
            header: EventHeader {
                event_type: EventType::SyscallAnomaly as u32,
                timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
                pid,
                tid: 0,
            },
            syscall_nr,
            syscall_category: category,
            count_in_window: suspicious_count,
        };
        EVENTS.output(&event, 0);
    }

    Ok(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
```

### Userspace Action on Syscall Anomaly

1. **Privilege escalation syscall from Wasm instance**: This is a **critical security
   incident**. The Wasm SFI boundary has been bypassed (hypothetical Wasmtime bug).
   Actions:
   - Immediately kill the instance (`JoinHandle::abort()`)
   - Log a `SECURITY` alert with the PID, syscall number, and app ID
   - Emit `wasm_security_syscall_violation_total` Prometheus counter
   - Publish `Event::SecurityIncident` to NATS (all nodes quarantine this artifact hash)
   - Write an audit log entry (Step 07 audit module)

2. **High syscall rate**: If a PID exceeds `syscall_rate_limit` (default: 100,000/sec),
   it's likely in a tight loop. The Supervisor reduces the instance's fuel allocation
   to throttle it, or kills it if the rate persists for 3 consecutive windows.

3. **execve from Wasm instance**: A Wasm instance should never call `execve`. This
   indicates either a Wasmtime bug or a compromised host. Kill the instance and log
   a `SECURITY` alert.

---

## 9. Userspace Loader & Ring Buffer Consumer

The userspace side of the eBPF monitor is a long-running Tokio task that:

1. Loads all eBPF programs at node startup
2. Attaches them to the appropriate tracepoints/kprobes
3. Reads events from the ring buffer
4. Dispatches events to action handlers
5. Exports metrics to Prometheus

### Monitor Configuration

```rust
// crates/ebpf-monitor/src/config.rs
use serde::{Deserialize, Serialize};

/// Configuration for the eBPF monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorConfig {
    /// Enable eBPF monitoring (requires Linux kernel >= 5.8).
    pub enabled: bool,

    /// FD soft limit per Wasm instance (warning at 80%).
    pub fd_soft_limit: u32,

    /// FD hard limit per Wasm instance (kill at 95%).
    pub fd_hard_limit: u32,

    /// Memory pressure low threshold (free pages).
    pub mem_low_threshold_pages: u64,

    /// Memory pressure critical threshold (free pages).
    pub mem_critical_threshold_pages: u64,

    /// Disk I/O latency threshold for "slow" alert (nanoseconds).
    pub disk_slow_threshold_ns: u64,

    /// Maximum TCP connections per PID before alert.
    pub tcp_conn_limit_per_pid: u32,

    /// Syscall rate limit per second for suspicious categories.
    pub syscall_rate_limit: u64,

    /// Sampling period for periodic counters (seconds).
    pub sampling_period_secs: u64,

    /// Enable individual eBPF programs.
    pub enable_process_tracker: bool,
    pub enable_tcp_monitor: bool,
    pub enable_fd_watcher: bool,
    pub enable_mem_pressure: bool,
    pub enable_disk_monitor: bool,
    pub enable_syscall_counter: bool,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        MonitorConfig {
            enabled: true,
            fd_soft_limit: 8192,         // 80% of default 1024 soft limit
            fd_hard_limit: 9728,         // 95% of 10240
            mem_low_threshold_pages: 65536,   // ~256 MB free
            mem_critical_threshold_pages: 16384, // ~64 MB free
            disk_slow_threshold_ns: 50_000_000, // 50 ms
            tcp_conn_limit_per_pid: 10000,
            syscall_rate_limit: 100_000,
            sampling_period_secs: 10,
            enable_process_tracker: true,
            enable_tcp_monitor: true,
            enable_fd_watcher: true,
            enable_mem_pressure: true,
            enable_disk_monitor: true,
            enable_syscall_counter: true,
        }
    }
}
```

### eBPF Loader

```rust
// crates/ebpf-monitor/src/loader.rs
use anyhow::Result;
use aya::{
    Ebpf,
    programs::{KProbe, TracePoint},
    maps::Array,
};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn, error};

use crate::config::MonitorConfig;
use crate::common::MonitorConfigMap;

/// Loaded eBPF programs and maps.
pub struct LoadedEbpf {
    pub ebpf: Ebpf,
    pub attached: Vec<aya::programs::ProgramId>,
}

/// Load and attach all eBPF programs.
pub async fn load_and_attach(config: &MonitorConfig, node_pid: u32) -> Result<Option<LoadedEbpf>> {
    if !config.enabled {
        info!("eBPF monitor disabled by configuration");
        return Ok(None);
    }

    // Check kernel version
    if !is_kernel_supported() {
        warn!("Kernel does not support required eBPF features — falling back to userspace");
        return Ok(None);
    }

    info!("Loading eBPF programs...");

    // Load the compiled eBPF object
    let mut ebpf = Ebpf::load(include_bytes_aligned!(
        "../../ebpf-monitor-bpf/target/bpfel-unknown-none/release/process_tracker"
    ))?;

    // Write config map
    let config_map: Array<_, MonitorConfigMap> = ebpf.map("CONFIG")
        .ok_or_else(|| anyhow::anyhow!("CONFIG map not found"))?
        .try_into()?;

    let kernel_config = MonitorConfigMap {
        node_pid,
        fd_soft_limit: config.fd_soft_limit,
        fd_hard_limit: config.fd_hard_limit,
        mem_low_threshold_pages: config.mem_low_threshold_pages,
        mem_critical_threshold_pages: config.mem_critical_threshold_pages,
        disk_slow_threshold_ns: config.disk_slow_threshold_ns,
        tcp_conn_limit_per_pid: config.tcp_conn_limit_per_pid,
        syscall_rate_limit: config.syscall_rate_limit,
        sampling_period_ns: config.sampling_period_secs * 1_000_000_000,
    };
    config_map.set(0, kernel_config, 0)?;

    let mut attached = Vec::new();

    // Attach process tracker
    if config.enable_process_tracker {
        attach_tracepoint(&mut ebpf, "sched_process_exec", "sched", "sched_process_exec")?;
        attach_tracepoint(&mut ebpf, "sched_process_exit", "sched", "sched_process_exit")?;
        info!("Process tracker attached");
    }

    // Attach TCP monitor
    if config.enable_tcp_monitor {
        attach_tracepoint(&mut ebpf, "inet_sock_set_state", "sock", "inet_sock_set_state")?;
        info!("TCP monitor attached");
    }

    // Attach FD watcher
    if config.enable_fd_watcher {
        attach_kprobe(&mut ebpf, "fd_install", "fd_install")?;
        attach_kprobe(&mut ebpf, "do_filp_close", "do_filp_close")?;
        info!("FD watcher attached");
    }

    // Attach memory pressure
    if config.enable_mem_pressure {
        attach_kprobe(&mut ebpf, "try_to_free_pages", "try_to_free_pages")?;
        info!("Memory pressure sentinel attached");
    }

    // Attach disk monitor
    if config.enable_disk_monitor {
        attach_tracepoint(&mut ebpf, "block_rq_issue", "block", "block_rq_issue")?;
        attach_tracepoint(&mut ebpf, "block_rq_complete", "block", "block_rq_complete")?;
        info!("Disk I/O monitor attached");
    }

    // Attach syscall counter
    if config.enable_syscall_counter {
        attach_tracepoint(&mut ebpf, "sys_enter", "raw_syscalls", "sys_enter")?;
        info!("Syscall counter attached");
    }

    info!("All eBPF programs loaded and attached successfully");

    Ok(Some(LoadedEbpf { ebpf, attached }))
}

fn attach_tracepoint(
    ebpf: &mut Ebpf,
    program_name: &str,
    category: &str,
    name: &str,
) -> Result<()> {
    let program: &mut TracePoint = ebpf.program_mut(program_name)
        .ok_or_else(|| anyhow::anyhow!("Program {} not found", program_name))?
        .try_into()?;
    program.load()?;
    program.attach(category, name)?;
    Ok(())
}

fn attach_kprobe(
    ebpf: &mut Ebpf,
    program_name: &str,
    fn_name: &str,
) -> Result<()> {
    let program: &mut KProbe = ebpf.program_mut(program_name)
        .ok_or_else(|| anyhow::anyhow!("Program {} not found", program_name))?
        .try_into()?;
    program.load()?;
    program.attach(fn_name, 0)?;
    Ok(())
}

fn is_kernel_supported() -> bool {
    // Check for BTF support, ring buffer support, and kernel >= 5.8
    // In production, read /proc/version_signature and check for BTF in /sys/kernel/btf/vmlinux
    match std::fs::metadata("/sys/kernel/btf/vmlinux") {
        Ok(_) => true,
        Err(_) => {
            warn!("BTF not available — eBPF programs may not load");
            false
        }
    }
}
```

### Ring Buffer Consumer & Action Dispatcher

```rust
// crates/ebpf-monitor/src/consumer.rs
use aya::maps::RingBuf as AyaRingBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn, error};

use crate::actions::RecoveryAction;
use crate::common::*;

/// Events read from the eBPF ring buffer, parsed and dispatched.
pub enum MonitorEvent {
    ProcessExec(ProcessEvent),
    ProcessExit(ProcessEvent),
    TcpConnect(TcpEvent),
    TcpClose(TcpEvent),
    TcpRetransmit(TcpEvent),
    FdOpen(FdEvent),
    FdLimitApproaching(FdEvent),
    MemPressure(MemPressureEvent),
    DiskSlowIo(DiskIoEvent),
    SyscallAnomaly(SyscallEvent),
}

/// Read events from the eBPF ring buffer and dispatch to action handlers.
pub async fn consume_ring_buffer(
    mut ring_buf: AyaRingBuf<AyaRingBuf>,
    action_tx: mpsc::Sender<MonitorEvent>,
) {
    loop {
        // Read available events from the ring buffer
        while let Some(item) = ring_buf.next() {
            if item.len() < std::mem::size_of::<EventHeader>() {
                continue;
            }

            let header: EventHeader = match read_struct(&item) {
                Ok(h) => h,
                Err(_) => continue,
            };

            let event = match header.event_type {
                t if t == EventType::ProcessExec as u32 => {
                    read_event::<ProcessEvent>(&item).map(MonitorEvent::ProcessExec)
                }
                t if t == EventType::ProcessExit as u32 => {
                    read_event::<ProcessEvent>(&item).map(MonitorEvent::ProcessExit)
                }
                t if t == EventType::TcpConnect as u32 => {
                    read_event::<TcpEvent>(&item).map(MonitorEvent::TcpConnect)
                }
                t if t == EventType::TcpClose as u32 => {
                    read_event::<TcpEvent>(&item).map(MonitorEvent::TcpClose)
                }
                t if t == EventType::TcpRetransmit as u32 => {
                    read_event::<TcpEvent>(&item).map(MonitorEvent::TcpRetransmit)
                }
                t if t == EventType::FdOpen as u32 => {
                    read_event::<FdEvent>(&item).map(MonitorEvent::FdOpen)
                }
                t if t == EventType::FdLimitApproaching as u32 => {
                    read_event::<FdEvent>(&item).map(MonitorEvent::FdLimitApproaching)
                }
                t if t == EventType::MemPressure as u32 => {
                    read_event::<MemPressureEvent>(&item).map(MonitorEvent::MemPressure)
                }
                t if t == EventType::DiskSlowIo as u32 => {
                    read_event::<DiskIoEvent>(&item).map(MonitorEvent::DiskSlowIo)
                }
                t if t == EventType::SyscallAnomaly as u32 => {
                    read_event::<SyscallEvent>(&item).map(MonitorEvent::SyscallAnomaly)
                }
                _ => {
                    warn!(event_type = header.event_type, "unknown eBPF event type");
                    continue;
                }
            };

            match event {
                Ok(e) => {
                    if action_tx.send(e).await.is_err() {
                        error!("action channel closed — eBPF monitor shutting down");
                        return;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "failed to parse eBPF event");
                }
            }
        }

        // Poll interval: 10ms (much faster than the 5s health loop)
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}

fn read_struct<T: Copy>(bytes: &[u8]) -> Result<T, anyhow::Error> {
    if bytes.len() < std::mem::size_of::<T>() {
        return Err(anyhow::anyhow!("buffer too small"));
    }
    Ok(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const T) })
}

fn read_event<T: Copy + Into<MonitorEvent>>(bytes: &[u8]) -> Result<T, anyhow::Error> {
    read_struct(bytes)
}
```

### Recovery Action Executor

```rust
// crates/ebpf-monitor/src/actions.rs
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, warn, error};

use crate::consumer::MonitorEvent;
use crate::metrics::EbpfMetrics;

/// Actions that the eBPF monitor can trigger.
pub enum RecoveryAction {
    /// Remove a dead instance from the upstream table immediately.
    RemoveFromUpstream { pid: u32 },
    /// Kill an instance that is consuming too many resources.
    KillInstance { pid: u32, reason: String },
    /// Activate backpressure (stop accepting new connections).
    ActivateBackpressure { reason: String },
    /// Deactivate backpressure (resume accepting connections).
    DeactivateBackpressure,
    /// Enter degraded mode (NATS partition likely).
    EnterDegradedMode { reason: String },
    /// Prune all idle instances to free memory/FDs.
    PruneIdleInstances,
    /// Security incident — quarantine an artifact.
    SecurityIncident { pid: u32, syscall_nr: u64, category: String },
    /// Log a warning (no automated action, but emit metric).
    WarnOnly { message: String },
}

/// Process monitor events and determine recovery actions.
pub struct ActionDispatcher {
    metrics: Arc<EbpfMetrics>,
    // Callbacks to interact with the rest of the platform
    backpressure: Arc<proxy::backpressure::BackpressureSignal>,
    nats_health: Arc<messaging::reconnect::NatsHealth>,
    event_tx: tokio::sync::mpsc::Sender<messaging::events::Event>,
    node_id: String,
}

impl ActionDispatcher {
    pub async fn dispatch(&self, event: MonitorEvent) {
        match event {
            MonitorEvent::ProcessExit(e) => {
                if e.signal == 9 {
                    // OOM kill
                    error!(
                        pid = e.header.pid,
                        ppid = e.ppid,
                        "OOM kill detected for wasm-node child process"
                    );
                    self.metrics.oom_kills.inc();
                    // The Supervisor's health loop will also detect this,
                    // but we can preemptively remove from upstream table.
                } else if e.signal != 0 {
                    warn!(
                        pid = e.header.pid,
                        signal = e.signal,
                        exit_code = e.exit_code,
                        "Wasm instance killed by signal"
                    );
                    self.metrics.signal_deaths.inc_by(1);
                }
                self.metrics.process_exits.inc();
            }

            MonitorEvent::TcpRetransmit(e) => {
                // If retransmits are on the NATS port, pre-emptively mark disconnected
                if e.dst_port == 4222 || e.src_port == 4222 {
                    warn!("NATS TCP retransmits detected — pre-emptive disconnect warning");
                    self.nats_health.mark_disconnected();
                    self.metrics.nats_retransmit_events.inc();
                }
                self.metrics.tcp_retransmits.inc();
            }

            MonitorEvent::FdLimitApproaching(e) => {
                warn!(
                    pid = e.header.pid,
                    fd_count = e.current_fd_count,
                    soft_limit = e.fd_soft_limit,
                    "FD limit approaching — pruning idle instances"
                );
                self.metrics.fd_usage_ratio.set(
                    e.current_fd_count as f64 / e.fd_soft_limit as f64
                );
            }

            MonitorEvent::MemPressure(e) => {
                match e.pressure_level {
                    0 => {
                        info!("Memory pressure: LOW");
                        self.metrics.memory_pressure_level.set(1.0);
                    }
                    1 => {
                        warn!("Memory pressure: MEDIUM — pruning idle instances");
                        self.metrics.memory_pressure_level.set(2.0);
                        // Trigger backpressure temporarily
                        self.backpressure.set_rejecting();
                        // Publish NodeUnderPressure event
                        let _ = self.event_tx.send(
                            messaging::events::Event::NodeUnderPressure {
                                node_id: self.node_id.clone(),
                                pressure_level: 1,
                            }
                        ).await;
                    }
                    2 => {
                        error!("Memory pressure: CRITICAL — killing largest instance");
                        self.metrics.memory_pressure_level.set(3.0);
                        self.backpressure.set_rejecting();
                        let _ = self.event_tx.send(
                            messaging::events::Event::NodeUnderPressure {
                                node_id: self.node_id.clone(),
                                pressure_level: 2,
                            }
                        ).await;
                    }
                    _ => {}
                }
            }

            MonitorEvent::DiskSlowIo(e) => {
                warn!(
                    dev = format!("{}:{}", e.dev_major, e.dev_minor),
                    latency_ms = e.latency_ns as f64 / 1_000_000.0,
                    "Slow disk I/O detected"
                );
                self.metrics.disk_io_latency_seconds.observe(
                    e.latency_ns as f64 / 1_000_000_000.0
                );
            }

            MonitorEvent::SyscallAnomaly(e) => {
                if e.syscall_category == SyscallCategory::PrivilegeEscalation as u32 {
                    error!(
                        pid = e.header.pid,
                        syscall_nr = e.syscall_nr,
                        count = e.count_in_window,
                        "SECURITY: Privilege escalation syscall from Wasm instance!"
                    );
                    self.metrics.security_violations.inc();
                    // Kill the offending instance immediately
                    // (would need a callback to the Supervisor)
                }
            }

            _ => {
                // Other events are informational — metrics only
            }
        }
    }
}
```

---

## 10. Prometheus Metrics

All eBPF-derived metrics are exported through the existing Prometheus registry (Step 11).

```rust
// crates/ebpf-monitor/src/metrics.rs
use prometheus::{
    IntCounter, IntGauge, Histogram, Opts, Registry, opts, histogram_opts,
};
use std::sync::Arc;

pub struct EbpfMetrics {
    /// Total OOM kills detected by eBPF.
    pub oom_kills: IntCounter,

    /// Total process exits detected (excluding OOM).
    pub process_exits: IntCounter,

    /// Total signal deaths (non-OOM signals).
    pub signal_deaths: IntCounter,

    /// Total TCP retransmits detected.
    pub tcp_retransmits: IntCounter,

    /// NATS retransmit events (subset of tcp_retransmits).
    pub nats_retransmit_events: IntCounter,

    /// FD usage ratio (current / soft limit) for the highest-FD PID.
    pub fd_usage_ratio: IntGauge,

    /// Memory pressure level (0=none, 1=low, 2=medium, 3=critical).
    pub memory_pressure_level: IntGauge,

    /// Disk I/O latency histogram.
    pub disk_io_latency_seconds: Histogram,

    /// Security violations (privileged syscalls from Wasm instances).
    pub security_violations: IntCounter,

    /// Whether eBPF is loaded and active (1=active, 0=fallback).
    pub ebpf_active: IntGauge,
}

impl EbpfMetrics {
    pub fn new(registry: &Registry) -> Self {
        let oom_kills = IntCounter::with_opts(Opts::new(
            "wasm_ebpf_oom_kills_total",
            "Total OOM kills detected by eBPF process tracker",
        )).unwrap();
        registry.register(Box::new(oom_kills.clone())).unwrap();

        let process_exits = IntCounter::with_opts(Opts::new(
            "wasm_ebpf_process_exits_total",
            "Total process exits detected by eBPF",
        )).unwrap();
        registry.register(Box::new(process_exits.clone())).unwrap();

        let signal_deaths = IntCounter::with_opts(Opts::new(
            "wasm_ebpf_signal_deaths_total",
            "Total signal deaths (non-OOM) detected by eBPF",
        )).unwrap();
        registry.register(Box::new(signal_deaths.clone())).unwrap();

        let tcp_retransmits = IntCounter::with_opts(Opts::new(
            "wasm_ebpf_tcp_retransmits_total",
            "Total TCP retransmits detected by eBPF",
        )).unwrap();
        registry.register(Box::new(tcp_retransmits.clone())).unwrap();

        let nats_retransmit_events = IntCounter::with_opts(Opts::new(
            "wasm_ebpf_nats_retransmits_total",
            "NATS TCP retransmit events detected by eBPF",
        )).unwrap();
        registry.register(Box::new(nats_retransmit_events.clone())).unwrap();

        let fd_usage_ratio = IntGauge::with_opts(Opts::new(
            "wasm_ebpf_fd_usage_ratio",
            "FD usage ratio (current/limit) for highest-FD PID",
        )).unwrap();
        registry.register(Box::new(fd_usage_ratio.clone())).unwrap();

        let memory_pressure_level = IntGauge::with_opts(Opts::new(
            "wasm_ebpf_memory_pressure_level",
            "Memory pressure level (0=none, 1=low, 2=medium, 3=critical)",
        )).unwrap();
        registry.register(Box::new(memory_pressure_level.clone())).unwrap();

        let disk_io_latency_seconds = Histogram::with_histogram_opts(histogram_opts!(
            "wasm_ebpf_disk_io_latency_seconds",
            "Disk I/O latency from eBPF block tracepoints",
            vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0]
        )).unwrap();
        registry.register(Box::new(disk_io_latency_seconds.clone())).unwrap();

        let security_violations = IntCounter::with_opts(Opts::new(
            "wasm_ebpf_security_violations_total",
            "Security violations detected by eBPF syscall monitor",
        )).unwrap();
        registry.register(Box::new(security_violations.clone())).unwrap();

        let ebpf_active = IntGauge::with_opts(Opts::new(
            "wasm_ebpf_active",
            "Whether eBPF monitoring is active (1=yes, 0=fallback)",
        )).unwrap();
        registry.register(Box::new(ebpf_active.clone())).unwrap();

        EbpfMetrics {
            oom_kills,
            process_exits,
            signal_deaths,
            tcp_retransmits,
            nats_retransmit_events,
            fd_usage_ratio,
            memory_pressure_level,
            disk_io_latency_seconds,
            security_violations,
            ebpf_active,
        }
    }
}
```

### Prometheus Alerting Rules

```yaml
# eBPF-based alerting rules (add to existing alertmanager config from Step 11)
groups:
  - name: ebpf_monitoring
    rules:
      - alert: EbpfOOMKill
        expr: rate(wasm_ebpf_oom_kills_total[5m]) > 0
        for: 0m
        annotations:
          summary: "OOM kill detected by eBPF on {{ $labels.node }}"
          description: "The Linux OOM killer terminated a process. Check memory pressure."

      - alert: EbpfMemoryPressureCritical
        expr: wasm_ebpf_memory_pressure_level == 3
        for: 30s
        annotations:
          summary: "Critical memory pressure on {{ $labels.node }}"
          description: "Node is about to trigger OOM killer. Instances are being killed proactively."

      - alert: EbpfNatsRetransmits
        expr: rate(wasm_ebpf_nats_retransmits_total[2m]) > 5
        for: 1m
        annotations:
          summary: "NATS TCP retransmits detected on {{ $labels.node }}"
          description: "Network degradation likely. NATS partition may be imminent."

      - alert: EbpfFdExhaustion
        expr: wasm_ebpf_fd_usage_ratio > 0.9
        for: 1m
        annotations:
          summary: "FD exhaustion approaching on {{ $labels.node }}"
          description: "File descriptor usage is above 90%. Node may soon refuse connections."

      - alert: EbpfSecurityViolation
        expr: rate(wasm_ebpf_security_violations_total[5m]) > 0
        for: 0m
        annotations:
          summary: "Security violation detected on {{ $labels.node }}"
          description: "A Wasm instance made a privileged syscall. Possible sandbox escape."

      - alert: EbpfDiskSlowIo
        expr: histogram_quantile(0.99, rate(wasm_ebpf_disk_io_latency_seconds_bucket[5m])) > 0.1
        for: 2m
        annotations:
          summary: "Slow disk I/O on {{ $labels.node }}"
          description: "P99 disk I/O latency exceeds 100ms. Redb performance may be degraded."
```

---

## 11. Userspace Fallback (Non-Linux)

On systems without eBPF support (Windows, macOS, old kernels), the monitor falls back
to userspace polling. This provides the same API surface (same metrics, same action
callbacks) but with higher latency and less precision.

```rust
// crates/ebpf-monitor/src/fallback.rs
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tracing::{info, warn};

use crate::config::MonitorConfig;
use crate::metrics::EbpfMetrics;
use crate::actions::ActionDispatcher;

/// Userspace fallback that reads /proc and /sys instead of eBPF.
pub async fn run_fallback_monitor(
    config: &MonitorConfig,
    metrics: Arc<EbpfMetrics>,
    dispatcher: Arc<ActionDispatcher>,
    node_pid: u32,
) {
    info!("Running eBPF monitor in userspace fallback mode (higher latency)");

    metrics.ebpf_active.set(0);

    let mut interval = time::interval(Duration::from_secs(5)); // Same as health loop

    loop {
        interval.tick().await;

        // Read /proc/meminfo for memory pressure
        if let Ok(meminfo) = read_meminfo() {
            let free_pages = meminfo.mem_available_kb * 1024 / 4096;
            let pressure_level = if free_pages < config.mem_critical_threshold_pages {
                2
            } else if free_pages < config.mem_low_threshold_pages {
                1
            } else {
                0
            };
            metrics.memory_pressure_level.set(pressure_level as f64);

            if pressure_level >= 2 {
                warn!("Memory pressure detected (userspace fallback): free_pages={}", free_pages);
            }
        }

        // Read /proc/<pid>/fd for FD count
        if let Ok(fd_count) = count_fds(node_pid) {
            let ratio = fd_count as f64 / config.fd_soft_limit as f64;
            metrics.fd_usage_ratio.set((ratio * 100.0) as i64);

            if ratio > 0.9 {
                warn!("FD usage high (userspace fallback): {}/{}", fd_count, config.fd_soft_limit);
            }
        }

        // Read /proc/diskstats for I/O latency (approximation)
        // Userspace cannot measure per-request latency — only aggregate stats
    }
}

struct MemInfo {
    mem_total_kb: u64,
    mem_available_kb: u64,
}

fn read_meminfo() -> Result<MemInfo, std::io::Error> {
    let content = std::fs::read_to_string("/proc/meminfo")?;
    let mut total = 0u64;
    let mut available = 0u64;
    for line in content.lines() {
        if line.starts_with("MemTotal:") {
            total = parse_kb(line);
        } else if line.starts_with("MemAvailable:") {
            available = parse_kb(line);
        }
    }
    Ok(MemInfo { mem_total_kb: total, mem_available_kb: available })
}

fn parse_kb(line: &str) -> u64 {
    line.split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn count_fds(pid: u32) -> Result<u32, std::io::Error> {
    let fd_dir = format!("/proc/{}/fd", pid);
    Ok(std::fs::read_dir(fd_dir)?.count() as u32)
}
```

---

## 12. Integration with Node Startup

The eBPF monitor is initialized during the node startup sequence (Step 14), after
storage and before the Supervisor starts the health loop.

```rust
// crates/ebpf-monitor/src/lib.rs
pub mod config;
pub mod loader;
pub mod consumer;
pub mod actions;
pub mod metrics;
pub mod fallback;

#[cfg(feature = "ebpf")]
pub mod common;

use config::MonitorConfig;
use metrics::EbpfMetrics;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Initialize the eBPF monitor subsystem.
/// Returns None if eBPF is not available (falls back to userspace polling).
pub async fn init(
    config: &MonitorConfig,
    metrics: Arc<EbpfMetrics>,
    dispatcher: Arc<actions::ActionDispatcher>,
    node_pid: u32,
) -> Option<tokio::task::JoinHandle<()>> {
    #[cfg(feature = "ebpf")]
    {
        match loader::load_and_attach(config, node_pid).await {
            Ok(Some(loaded)) => {
                metrics.ebpf_active.set(1);

                // Open the ring buffer
                let ring_buf = loaded.ebpf.map("EVENTS")
                    .and_then(|m| aya::maps::RingBuf::try_from(m).ok())?;

                let (action_tx, mut action_rx) = mpsc::channel::<consumer::MonitorEvent>(4096);

                // Spawn ring buffer consumer
                let consumer_handle = tokio::spawn(async move {
                    consumer::consume_ring_buffer(ring_buf, action_tx).await;
                });

                // Spawn action dispatcher
                let disp = dispatcher.clone();
                tokio::spawn(async move {
                    while let Some(event) = action_rx.recv().await {
                        disp.dispatch(event).await;
                    }
                });

                info!("eBPF monitor initialized and running");
                return Some(consumer_handle);
            }
            Ok(None) => {
                warn!("eBPF not available — running in userspace fallback mode");
            }
            Err(e) => {
                warn!(error = %e, "eBPF load failed — running in userspace fallback mode");
            }
        }
    }

    #[cfg(not(feature = "ebpf"))]
    {
        warn!("eBPF feature not compiled — running in userspace fallback mode");
    }

    // Fallback: userspace polling
    let fallback_config = config.clone();
    let fallback_metrics = metrics.clone();
    let fallback_dispatcher = dispatcher.clone();
    let handle = tokio::spawn(async move {
        fallback::run_fallback_monitor(
            &fallback_config,
            fallback_metrics,
            fallback_dispatcher,
            node_pid,
        ).await;
    });

    Some(handle)
}
```

### Node main.rs Integration

```rust
// crates/node/src/main.rs — addition to startup sequence (after step 18, before health loop)

// 18.5. Initialize eBPF monitor
let ebpf_config = ebpf_monitor::config::MonitorConfig::default();
let ebpf_metrics = Arc::new(ebpf_monitor::metrics::EbpfMetrics::new(&prom_metrics.registry));
let ebpf_dispatcher = Arc::new(ebpf_monitor::actions::ActionDispatcher {
    metrics: ebpf_metrics.clone(),
    backpressure: backpressure.clone(),
    nats_health: nats_health.clone(),
    event_tx: event_tx.clone(),
    node_id: args.node_id.clone(),
});
let node_pid = std::process::id();
let _ebpf_handle = ebpf_monitor::init(
    &ebpf_config,
    ebpf_metrics.clone(),
    ebpf_dispatcher.clone(),
    node_pid,
).await;
```

---

## 13. NATS Event Extensions

The eBPF monitor publishes new event types to the NATS control plane so other nodes
can react to kernel-level observations.

```rust
// crates/messaging/src/events.rs — additions to the Event enum

pub enum Event {
    // ... existing events ...

    /// A node is under memory or I/O pressure.
    /// Other nodes should stop steering traffic to it.
    NodeUnderPressure {
        node_id: String,
        /// 1 = medium, 2 = critical
        pressure_level: u32,
    },

    /// A node recovered from pressure.
    NodePressureRecovered {
        node_id: String,
    },

    /// Security incident: a Wasm instance made a privileged syscall.
    SecurityIncident {
        node_id: String,
        app_id: String,
        pid: u32,
        syscall_nr: u64,
        category: String,
    },
}
```

### Peer Node Reaction to `NodeUnderPressure`

When a node receives `NodeUnderPressure`:

```rust
// In the event dispatcher (crates/node/src/handlers.rs)
async fn handle_node_under_pressure(node_id: &str, pressure_level: u32) {
    // Remove the pressured node from the load table
    // so no new requests are steered to it
    node_table.mark_unhealthy(node_id);

    if pressure_level >= 2 {
        // Critical: also remove all upstream entries for this node
        tracing::warn!(
            node = node_id,
            "Node under critical pressure — removing from routing"
        );
    }
}
```

---

## 14. CLI Commands

```
# Check eBPF monitor status
wasm-ctl node ebpf-status --node node-0
# Output:
# eBPF Active: YES
# Programs loaded: 6/6
# Kernel version: 6.1.0
# BTF available: YES
# Events processed: 145,203
# Last event: 2ms ago (ProcessExit, pid=12345)
#
# Program details:
#   process_tracker: ATTACHED (tracepoint sched_process_exec, sched_process_exit)
#   tcp_monitor:     ATTACHED (tracepoint inet_sock_set_state)
#   fd_watcher:      ATTACHED (kprobe fd_install, do_filp_close)
#   mem_pressure:    ATTACHED (kprobe try_to_free_pages)
#   disk_monitor:    ATTACHED (tracepoint block_rq_issue, block_rq_complete)
#   syscall_counter: ATTACHED (tracepoint raw_syscalls/sys_enter)

# View current eBPF metrics
wasm-ctl node ebpf-metrics --node node-0
# Output:
# Memory pressure: LOW (level 1)
# FD usage: 234/8192 (2.9%)
# TCP connections: 1,203
# Disk I/O p99 latency: 12ms
# OOM kills: 0
# Security violations: 0
# NATS retransmits: 0

# Dynamically adjust eBPF thresholds
wasm-ctl node ebpf-config --node node-0 --fd-soft-limit 4096 --mem-critical-pages 8192

# Disable a specific eBPF program at runtime
wasm-ctl node ebpf-disable --node node-0 --program syscall_counter

# Enable a specific eBPF program at runtime
wasm-ctl node ebpf-enable --node node-0 --program syscall_counter
```

---

## 15. Testing Strategy

### Unit Tests (No Kernel Required)

```bash
# Test event parsing, config validation, action dispatch logic
cargo test -p ebpf-monitor --lib
```

Unit tests cover:
- `MonitorConfig` validation (thresholds, limits)
- Event struct parsing from byte buffers
- Action dispatcher logic (given event X, expect action Y)
- Metrics registration and increment
- Fallback monitor `/proc` parsing

### Integration Tests (Require Linux + eBPF)

```bash
# Test eBPF program loading and attachment
cargo test -p ebpf-monitor --tests --features ebpf
```

Integration tests cover:
- eBPF program loads without verifier errors
- Ring buffer produces events when triggered
- Config map is readable from eBPF programs
- Detach and reattach works correctly

### E2E Tests (Full Cluster)

```bash
# Test eBPF monitor in a running cluster
cargo test -p e2e -- --ignored --test-threads=1
```

E2E tests for eBPF:

1. **`test_ebpf_detects_oom_kill`**: Deploy a Wasm app with low memory limit. Trigger
   OOM. Verify eBPF detects the OOM kill before the health loop and the instance is
   removed from the upstream table immediately.

2. **`test_ebpf_memory_pressure_triggers_backpressure`**: Deploy many instances until
   memory pressure reaches "medium". Verify backpressure signal activates and
   `NodeUnderPressure` event is published to NATS.

3. **`test_ebpf_fd_exhaustion_recovery`**: Open many file descriptors from a Wasm app
   (via WASI). Verify eBPF detects approaching FD limit and triggers instance pruning.

4. **`test_ebpf_nats_retransmit_early_warning`**: Simulate network degradation (tc netem
   delay + loss). Verify eBPF detects TCP retransmits on the NATS connection before
   the NatsHealthWatcher reports disconnection.

5. **`test_ebpf_syscall_violation`**: From a test process in the same cgroup as a Wasm
   instance, make a `bpf()` syscall. Verify eBPF detects it and logs a security alert.

6. **`test_ebpf_fallback_mode`**: Run the node with `--no-ebpf` flag. Verify userspace
   fallback provides the same metrics (with higher latency).

7. **`test_ebpf_disk_slow_io`**: Use `tc netem` or `dmsetup` to add artificial disk
   latency. Verify eBPF detects slow I/O and the node enters degraded mode.

---

## 16. Security Considerations

### eBPF Program Verification

All eBPF programs are verified by the Linux kernel before loading. The verifier ensures:
- No unbounded loops (bounded iteration only)
- No out-of-bounds memory access
- No kernel state modification (read-only unless using BPF LSM)
- Programs terminate in a bounded number of instructions

This means a bug in our eBPF program cannot crash the kernel or corrupt kernel state.

### Least-Privilege Attachment

eBPF programs require `CAP_BPF` or `CAP_SYS_ADMIN` to load. The `wasm-node` binary
should be granted the minimum capabilities needed:

```bash
# systemd unit file for wasm-node
[Service]
ExecStart=/opt/wasm-node/wasm-node --node-id node-0
AmbientCapabilities=CAP_BPF CAP_NET_ADMIN CAP_PERFMON
# CAP_BPF: load eBPF programs
# CAP_NET_ADMIN: attach network tracepoints
# CAP_PERFMON: read perf events
```

If the binary lacks these capabilities, eBPF loading fails gracefully and the
monitor falls back to userspace mode.

### Ring Buffer Isolation

The ring buffer is shared between kernel (producer) and userspace (consumer). The
kernel writes fixed-size structs; the userspace reads them. There is no bidirectional
data flow through the ring buffer — the kernel cannot be influenced by userspace
corrupting the buffer (the kernel only writes, never reads from it).

### Syscall Monitor: No False Positives

The syscall counter monitors Wasm instance threads. These threads should only make
syscalls through Wasmtime's WASI layer. If a thread makes a direct `ptrace` or `bpf`
syscall, it is either:
1. A Wasmtime bug (SFI bypass) — critical security incident
2. A host process accidentally in the same cgroup — misconfiguration

To avoid false positives, the `MONITORED_PIDS` map is populated only with PIDs that
the Supervisor explicitly registers as Wasm instances. Non-Wasm threads in the same
process (Tokio workers, NATS subscriber) are not monitored for syscalls.

### No eBPF Program Modification at Runtime

Once loaded, eBPF programs cannot be modified. The config map can be updated (thresholds),
but the program logic is immutable. This prevents an attacker from modifying the monitor
to hide their activity. To update eBPF programs, the node must be restarted (which
triggers the binary verification from Step 13).

---

## 17. Performance Impact

### Overhead per Event

```
eBPF Program          │ Hook Point                │ Overhead per Event
──────────────────────┼───────────────────────────┼────────────────────
process_tracker       │ sched_process_exec/exit    │ ~200ns (rare events)
tcp_monitor           │ inet_sock_set_state        │ ~500ns (per TCP state change)
fd_watcher            │ fd_install / filp_close    │ ~300ns (per fd operation)
mem_pressure          │ try_to_free_pages          │ ~400ns (only during reclaim)
disk_monitor          │ block_rq_issue/complete    │ ~300ns (per I/O request)
syscall_counter       │ raw_syscalls/sys_enter     │ ~100ns (every syscall!)
```

The syscall counter has the highest overhead because it fires on **every syscall**.
For a node handling 10,000 requests/second with ~100 syscalls per request, that's
1,000,000 events/second × 100ns = 100ms of CPU time per second (10% of one core).

This is acceptable for a security-critical monitor, but operators can disable it
via `enable_syscall_counter: false` if the overhead is too high.

### Ring Buffer Throughput

The ring buffer is 1 MB per program (6 MB total). At peak load:
- Process events: ~10/sec (negligible)
- TCP events: ~1,000/sec (spikes during connection storms)
- FD events: ~5,000/sec (spikes during instance spawn)
- Memory events: ~1/sec (only during pressure)
- Disk events: ~10,000/sec (busy redb)
- Syscall events: ~1,000,000/sec (if enabled)

The userspace consumer reads in 10ms intervals. At 1M events/sec, the consumer
processes ~10,000 events per tick. Each event takes ~1μs to parse and dispatch,
so the consumer uses ~10ms per 10ms tick — 100% of one core during peak syscall
monitoring.

**Mitigation**: The syscall counter uses a sampling approach — it only sends an
event to the ring buffer when a suspicious syscall is detected. Normal syscalls
increment a per-CPU counter but don't generate ring buffer events. This reduces
ring buffer traffic from 1M/sec to ~10/sec in normal operation.

### Memory Overhead

```
Component           │ Memory
────────────────────┼──────────────
eBPF programs       │ ~200 KB (code + maps)
Ring buffers        │ ~6 MB (1 MB × 6 programs)
Per-CPU maps        │ ~2 MB (depends on CPU count)
Userspace buffers   │ ~4 MB (channel, parsed events)
────────────────────┼──────────────
Total               │ ~12 MB
```

12 MB is negligible compared to the Wasm instance memory (each instance gets 64–256 MB).

---

## 18. Relationship to Existing Steps

```
Step │ eBPF Enhancement
─────┼────────────────────────────────────────────────────────────────────
  07 │ Process exit detected in <1ms vs 5s health loop. OOM kills flagged
    │ immediately. Instance removed from upstream table before 502 errors.
─────┼────────────────────────────────────────────────────────────────────
  11 │ New Prometheus metrics from kernel events. Sub-millisecond
    │ resolution vs 1-minute aggregation. New alerting rules.
─────┼────────────────────────────────────────────────────────────────────
  13 │ Defense in depth: syscall monitoring beyond Wasm SFI. Detects
    │ hypothetical Wasmtime sandbox escapes at the kernel level.
─────┼────────────────────────────────────────────────────────────────────
  24 │ Kernel-level connection counting complements userspace rate limiter.
    │ Backpressure triggered proactively on memory/FD pressure.
─────┼────────────────────────────────────────────────────────────────────
  27 │ L5 (partition) detected 5–30s earlier via TCP retransmits.
    │ L3 (corruption) predicted via disk I/O anomaly detection.
    │ Automated recovery actions replace manual CLI commands for
    │ memory pressure, FD exhaustion, and connection storms.
─────┼────────────────────────────────────────────────────────────────────
  20 │ Graceful shutdown triggered by memory pressure before OOM killer
    │ fires. Instance drain happens while the process is still healthy.
─────┼────────────────────────────────────────────────────────────────────
  26 │ GC triggered proactively when disk I/O latency increases
    │ (early sign of redb file growing too large).
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Crate Structure
- [ ] `crates/ebpf-monitor/` created with `Cargo.toml` (feature-gated `ebpf` feature)
- [ ] `crates/ebpf-monitor/bpf/` created with eBPF target configuration
- [ ] Workspace `Cargo.toml` updated with new member and `aya` dependency
- [ ] `cargo build --workspace` succeeds with and without `--features ebpf`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes

### Shared Data Structures
- [ ] `common.rs` defines all `#[repr(C)]` structs for kernel↔userspace communication
- [ ] `EventType` enum covers all 10 event types
- [ ] `MonitorConfigMap` struct matches the eBPF config map layout
- [ ] Structs compile in both `bpfel-unknown-none` and userspace targets
- [ ] No Rust-specific types (`String`, `Vec`, `Option`) in shared structs

### eBPF Program: Process Tracker
- [ ] `sched_process_exec` tracepoint handler filters by `ppid == node_pid`
- [ ] `sched_process_exit` tracepoint handler detects OOM kills (`signal == 9`)
- [ ] Events written to ring buffer with correct `EventType`
- [ ] Program compiles without verifier errors on kernel >= 5.8
- [ ] cgroup ID included in events for tenant attribution

### eBPF Program: TCP Connection Monitor
- [ ] `inet_sock_set_state` tracepoint handler tracks TCP state transitions
- [ ] Per-PID connection counter in `TCP_CONN_COUNT` hash map
- [ ] Connection limit exceeded event emitted when count > `tcp_conn_limit_per_pid`
- [ ] Retransmit counter tracks cumulative retransmits per PID
- [ ] NATS port (4222) retransmits flagged as `TcpRetransmit` event type
- [ ] Connection count decremented on `TCP_CLOSE` state transition

### eBPF Program: File Descriptor Watcher
- [ ] `fd_install` kprobe increments per-PID FD counter
- [ ] `do_filp_close` kprobe decrements per-PID FD counter
- [ ] `FdLimitApproaching` event emitted when count >= `fd_soft_limit`
- [ ] Hard limit event emitted when count >= `fd_hard_limit`
- [ ] FD counter accurate within ±5 (race condition tolerance)

### eBPF Program: Memory Pressure Sentinel
- [ ] `try_to_free_pages` kprobe detects direct reclaim
- [ ] `vmpressure_level_change` tracepoint detects kernel pressure notifications
- [ ] Pressure level deduplicated (only send on level change)
- [ ] Three levels emitted: low (1), medium (2), critical (3)
- [ ] cgroup-scoped pressure tracking for per-tenant attribution

### eBPF Program: Disk I/O Monitor
- [ ] `block_rq_issue` tracepoint records I/O start time per sector
- [ ] `block_rq_complete` tracepoint calculates latency
- [ ] `DiskSlowIo` event emitted when latency > `disk_slow_threshold_ns`
- [ ] Start time entries cleaned up on completion
- [ ] Device major:minor included in events for device identification

### eBPF Program: Syscall Anomaly Detector
- [ ] `raw_syscalls/sys_enter` tracepoint counts syscalls per PID
- [ ] `MONITORED_PIDS` hash map populated by process tracker
- [ ] Privileged syscalls (`ptrace`, `bpf`, `mount`, `setuid`) flagged
- [ ] `SyscallAnomaly` event emitted for suspicious categories
- [ ] Sampling approach: normal syscalls increment counter only (no ring buffer write)
- [ ] Per-CPU counters for low-contention counting

### Userspace Loader
- [ ] `load_and_attach()` loads all 6 eBPF programs
- [ ] Config map written with `MonitorConfigMap` before program attachment
- [ ] Kernel version and BTF support checked before loading
- [ ] Graceful failure: returns `None` if eBPF unavailable
- [ ] Individual programs can be disabled via `MonitorConfig` flags
- [ ] Loading logged with program names and attachment points

### Ring Buffer Consumer
- [ ] Ring buffer read in 10ms polling loop
- [ ] Events parsed by `EventType` discriminant
- [ ] Parsed events sent to action dispatcher via `mpsc` channel
- [ ] Malformed events logged and skipped (no panic)
- [ ] Consumer task shuts down cleanly when channel closes

### Action Dispatcher
- [ ] `ProcessExit` with `signal == 9` triggers OOM kill handling
- [ ] `TcpRetransmit` on NATS port triggers `NatsHealth::mark_disconnected()`
- [ ] `FdLimitApproaching` triggers idle instance pruning
- [ ] `MemPressure` level 2 triggers backpressure + `NodeUnderPressure` event
- [ ] `MemPressure` level 3 triggers instance killing + backpressure
- [ ] `DiskSlowIo` triggers degraded mode for redb writes
- [ ] `SyscallAnomaly` with `PrivilegeEscalation` triggers instance kill + audit log
- [ ] All actions logged with appropriate severity levels

### Prometheus Metrics
- [ ] `wasm_ebpf_oom_kills_total` counter
- [ ] `wasm_ebpf_process_exits_total` counter
- [ ] `wasm_ebpf_signal_deaths_total` counter
- [ ] `wasm_ebpf_tcp_retransmits_total` counter
- [ ] `wasm_ebpf_nats_retransmits_total` counter
- [ ] `wasm_ebpf_fd_usage_ratio` gauge
- [ ] `wasm_ebpf_memory_pressure_level` gauge
- [ ] `wasm_ebpf_disk_io_latency_seconds` histogram
- [ ] `wasm_ebpf_security_violations_total` counter
- [ ] `wasm_ebpf_active` gauge (1=eBPF, 0=fallback)
- [ ] All metrics registered with the existing Prometheus registry

### Prometheus Alerting Rules
- [ ] `EbpfOOMKill` alert fires on any OOM kill
- [ ] `EbpfMemoryPressureCritical` alert fires at pressure level 3
- [ ] `EbpfNatsRetransmits` alert fires on sustained NATS retransmits
- [ ] `EbpfFdExhaustion` alert fires at 90% FD usage
- [ ] `EbpfSecurityViolation` alert fires on any security violation
- [ ] `EbpfDiskSlowIo` alert fires when p99 disk latency > 100ms

### NATS Event Extensions
- [ ] `Event::NodeUnderPressure` defined with `node_id` and `pressure_level`
- [ ] `Event::NodePressureRecovered` defined with `node_id`
- [ ] `Event::SecurityIncident` defined with `node_id`, `app_id`, `pid`, `syscall_nr`
- [ ] Peer nodes remove pressured node from load table on `NodeUnderPressure`
- [ ] Peer nodes restore node in load table on `NodePressureRecovered`
- [ ] All nodes log `SecurityIncident` and quarantine artifact hash

### Userspace Fallback
- [ ] `/proc/meminfo` read for memory pressure detection
- [ ] `/proc/<pid>/fd` count for FD usage tracking
- [ ] `/proc/diskstats` read for disk I/O approximation
- [ ] Same Prometheus metrics exported (with `ebpf_active = 0`)
- [ ] Same action dispatcher called (with higher latency)
- [ ] Fallback runs on 5-second interval (same as health loop)

### Node Integration
- [ ] `ebpf_monitor::init()` called in `main.rs` startup sequence
- [ ] eBPF monitor starts after storage, before health loop
- [ ] `node_pid` passed from `std::process::id()`
- [ ] Backpressure signal shared between proxy and eBPF monitor
- [ ] NatsHealth shared between messaging and eBPF monitor
- [ ] Event channel shared between eBPF monitor and NATS publisher
- [ ] Node starts successfully with and without `--features ebpf`

### CLI Commands
- [ ] `wasm-ctl node ebpf-status` shows program status and event counts
- [ ] `wasm-ctl node ebpf-metrics` shows current metric values
- [ ] `wasm-ctl node ebpf-config` adjusts thresholds at runtime
- [ ] `wasm-ctl node ebpf-disable --program <name>` detaches a program
- [ ] `wasm-ctl node ebpf-enable --program <name>` reattaches a program

### Unit Tests
- [ ] `MonitorConfig` validation tests (invalid thresholds rejected)
- [ ] Event struct parsing tests (valid and malformed byte buffers)
- [ ] Action dispatcher unit tests (event → expected action mapping)
- [ ] Metrics registration tests (no duplicate metric names)
- [ ] Fallback `/proc` parsing tests (with mock files)

### Integration Tests
- [ ] eBPF programs load without verifier errors on kernel >= 5.8
- [ ] Ring buffer produces events when triggered by test actions
- [ ] Config map readable from eBPF programs
- [ ] Program detach and reattach works
- [ ] Graceful failure on unsupported kernel (returns None)

### E2E Tests
- [ ] `test_ebpf_detects_oom_kill`: OOM detected before health loop
- [ ] `test_ebpf_memory_pressure_triggers_backpressure`: Backpressure activates
- [ ] `test_ebpf_fd_exhaustion_recovery`: Instance pruning triggered
- [ ] `test_ebpf_nats_retransmit_early_warning`: Early partition detection
- [ ] `test_ebpf_syscall_violation`: Security incident logged
- [ ] `test_ebpf_fallback_mode`: Userspace fallback provides same metrics
- [ ] `test_ebpf_disk_slow_io`: Degraded mode entered on slow I/O

### Security
- [ ] eBPF programs verified by kernel (no manual verification needed)
- [ ] `CAP_BPF`, `CAP_NET_ADMIN`, `CAP_PERFMON` documented in systemd unit
- [ ] Graceful degradation when capabilities missing
- [ ] `MONITORED_PIDS` map scoped to Wasm instance PIDs only
- [ ] Ring buffer is write-only from kernel (no userspace→kernel data flow)
- [ ] No eBPF program modification at runtime (immutable after load)

### Documentation
- [ ] `AGENTS.md` updated with eBPF build commands
- [ ] `AGENTS.md` updated with eBPF test commands
- [ ] `00_OVERVIEW.md` updated with eBPF monitor in architecture diagram
- [ ] systemd unit file example with required capabilities
- [ ] Troubleshooting guide: "eBPF programs failed to load" scenarios

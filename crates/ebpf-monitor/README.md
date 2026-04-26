# ebpf-monitor

## Overview

Kernel-level observability module using eBPF (Linux >= 5.8) or userspace fallback. Monitors process lifecycle, TCP connections, file descriptors, memory pressure, disk I/O latency, and syscall anomalies. Events are dispatched through `ActionDispatcher`, which updates Prometheus metrics and triggers recovery actions via the `EventCallbacks` trait.

## Architecture

The monitor operates in two modes:

1. **eBPF mode** (Linux >= 5.8) — Attaches kprobes/tracepoints to kernel functions for low-overhead, real-time event collection. Loaded eBPF objects are held in `LoadedEbpf` and programs are attached on startup.
2. **Userspace fallback** — Polls `/proc` and `/sys` at a fixed interval when eBPF is unavailable (non-Linux or insufficient kernel version).

Events flow from the kernel → eBPF programs → Rust callback handlers → `ActionDispatcher` → `EventCallbacks` implementation. The dispatcher updates Prometheus metrics and invokes callback methods for recovery actions (e.g., restart, scale, alert).

### Monitored Subsystems

| Subsystem | eBPF Attach Point | Fallback Source |
|---|---|---|
| Process lifecycle | `sched_process_exec`, `sched_process_exit` | `/proc` scan |
| TCP connections | `tcp_set_state` kprobe | `/proc/net/tcp` |
| File descriptors | `fd_install` kprobe | `/proc/<pid>/fd` count |
| Memory pressure | `out_of_memory` kprobe (commented out) | `/proc/meminfo` |
| Disk I/O latency | `block_rq_issue`, `block_rq_complete` tracepoints | `/proc/diskstats` |
| Syscall anomalies | `raw_syscalls:sys_enter` tracepoint | Not available |

## Public API

### Key Types

| Type | Description |
|---|---|
| `MonitorHandle` | Handle to the running monitor; supports threshold updates and graceful shutdown |
| `MonitorStatus` | Current status snapshot (running/stopped, event counts, last error) |
| `MonitorConfig` | Configuration: thresholds, poll interval, enabled subsystems |
| `EbpfMetrics` | Prometheus metrics registry exposed by the monitor |
| `ActionDispatcher` | Central dispatcher: receives events, updates metrics, calls `EventCallbacks` |
| `EventCallbacks` | Trait for recovery actions triggered by threshold violations |
| `NoopCallbacks` | No-op implementation of `EventCallbacks` for testing |
| `MonitorEvent` | Enum of all monitored event types |
| `RecoveryAction` | Enum of possible recovery actions (restart, scale, alert, etc.) |
| `LoadedEbpf` | Holds loaded eBPF object references and attached program links |

### Example

```rust
let config = MonitorConfig::default();
let callbacks = MyCallbacks; // implements EventCallbacks
let handle = ebpf_monitor::start(config, callbacks)?;

// Update thresholds at runtime
handle.update_thresholds(new_thresholds)?;

// Graceful shutdown
handle.shutdown().await?;
```

## Known Issues & Improvements

### Reliability

- **`update_thresholds`/`current_config` use `unwrap()` on `RwLock`** — Will panic if the lock is poisoned after a thread panic. Should use `lock().unwrap_or_else(|e| e.into_inner())` or propagate the error.
- **`register_metric!` macro creates orphaned metrics on registration failure** — If Prometheus registration fails, the metric variable is left in an inconsistent state. The macro should return an error or retry.
- **`PENDING_IO_COUNT` can become inconsistent** — Missed `block_rq_complete` events leave stale entries, causing latency calculations to drift. Consider periodic garbage collection of old entries.
- **`parse_event` uses `unsafe read_unaligned` without data validation** — Malformed eBPF event data could cause undefined behavior. Add validation before reading.

### Correctness

- **TCP/FD count tracking includes ALL processes** — Not scoped to wasm-node children only. On busy hosts, counts will be meaningless. Filter by cgroup or parent PID.
- **`dev_minor` extraction incorrect for extended `dev_t` format** — Uses bit shift/mask that doesn't account for the kernel's split major/minor encoding. Use `major()`/`minor()` from `libc`.
- **`block_rq_requeue` handler commented out** — Causes stale entries in `IO_START_TIME` map, inflating latency measurements. Re-enable or add cleanup logic.
- **`out_of_memory` kprobe commented out** — OOM events are not detected in eBPF mode. Re-enable with appropriate safety checks.

### Production Readiness

- **Hardcoded eBPF object paths** — Points to local development paths. Won't work in production deployments. Use `include_bytes_aligned!` or resolve relative to the executable.
- **`include_bytes_aligned!` production path not implemented** — The compile-time inclusion of eBPF objects is stubbed out.
- **Fallback monitor hardcodes 5-second poll interval** — Should be configurable via `MonitorConfig`.
- **`syscall_counter` monitors ALL syscalls on system** — Significant performance impact on production hosts. Filter by PID namespace or cgroup.

### Performance

- **Per-endpoint rate limiting not implemented** — High-frequency events can overwhelm the dispatcher.
- **No rate limiting on `dispatch` method** — A burst of kernel events can saturate the callback pipeline. Add a token bucket or debounce.

## Security Considerations

- **`unsafe read_unaligned` in `parse_event`** — eBPF event buffers come from kernel maps. While typically trustworthy, a corrupted map or privileged attacker with access to BPF map memory could inject malformed data. Validate structure and bounds before unsafe reads.
- **eBPF requires `CAP_BPF` / `CAP_SYS_ADMIN`** — The wasm-node process must run with elevated capabilities. Consider dropping capabilities after loading programs, or using a separate privileged sidecar for eBPF.
- **`syscall_counter` visibility** — Monitoring all syscalls system-wide exposes information about other tenants' workloads on shared hosts. Scope to the node's cgroup.
- **No authentication on threshold update API** — Any code with a `MonitorHandle` can change thresholds, potentially suppressing alerts. Consider access control or audit logging for threshold changes.
```

<file_path>
Wasm-Cloud-Platform\crates\ebpf-monitor\README.md
</file_path>

<edit_description>
Create README.md for the ebpf-monitor crate
</edit_description>

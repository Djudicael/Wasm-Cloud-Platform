# eBPF Kernel Monitoring & Security

This guide covers the platform's eBPF-based kernel monitoring system — what it monitors, how it works, how to configure it, and how to respond to the events it generates.

## Table of Contents

1. [What is eBPF?](#what-is-ebpf)
2. [Why eBPF on This Platform](#why-ebpf-on-this-platform)
3. [Architecture Overview](#architecture-overview)
4. [eBPF Programs](#ebpf-programs)
5. [Configuration](#configuration)
6. [Operational Commands](#operational-commands)
7. [Security Incident Response](#security-incident-response)
8. [Monitoring eBPF Itself](#monitoring-ebpf-itself)
9. [Troubleshooting](#troubleshooting)
10. [Non-Linux Fallback](#non-linux-fallback)
11. [Performance Impact](#performance-impact)

---

## What is eBPF?

eBPF (extended Berkeley Packet Filter) is a technology that allows running sandboxed programs inside the Linux kernel without changing kernel source code or loading kernel modules. eBPF programs are:

- **Verified**: The kernel checks every eBPF program before loading it to ensure it cannot crash the kernel, hang, or access arbitrary memory
- **Event-driven**: eBPF programs run in response to kernel events (syscalls, network packets, tracepoints) rather than polling
- **High-performance**: They run directly in kernel space with minimal overhead
- **Safe**: A bug in an eBPF program cannot crash the host — the verifier rejects unsafe code

On this platform, eBPF provides **sub-millisecond detection** of failures that userspace polling would detect only after seconds or minutes.

---

## Why eBPF on This Platform

### Detection Latency Comparison

| Failure | Userspace Detection | eBPF Detection | Speedup |
|---------|---------------------|----------------|---------|
| Instance crash (OOM/trap) | 0–5s (health loop) | <1ms (tracepoint) | 5000x |
| NATS disconnection | 5–30s (heartbeat) | <1ms (TCP retransmit) | 30x |
| Memory pressure | 1–5min (Prometheus alert) | <1ms (vmpressure) | 300x |
| FD exhaustion | At failure (accept() fails) | ~1ms (fd_install kprobe) | Immediate |
| Syscall anomaly | Never (WASI SFI only) | <1ms (sys_enter tracepoint) | New capability |
| Disk I/O saturation | 5–10min (Prometheus alert) | ~1ms (block_rq_complete) | 600x |

### What eBPF Enables

1. **Preemptive recovery**: Detect memory pressure *before* the OOM killer fires, allowing proactive instance pruning
2. **Zero-day syscall detection**: Catch hypothetical Wasmtime sandbox escapes at the kernel level
3. **Network partition prediction**: Detect TCP retransmits on the NATS connection before the heartbeat fails
4. **FD leak detection**: Identify file descriptor leaks before they cause `EMFILE` failures
5. **No external agents**: eBPF programs load from within `wasm-node` itself — no Datadog, Falco, or Tetragon required

---

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                         Linux Kernel (5.8+)                                  │
│                                                                              │
│   ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐             │
│   │ Process Tracker │  │  TCP Monitor    │  │  FD Watcher     │             │
│   │ (tracepoint)    │  │ (tracepoint)    │  │ (kprobe)        │             │
│   └────────┬────────┘  └────────┬────────┘  └────────┬────────┘             │
│            │                    │                    │                        │
│   ┌────────┴────────────────────┴────────────────────┴────────┐             │
│   │                     Ring Buffer (perfbuf)                  │             │
│   │              1MB shared buffer, lock-free                 │             │
│   └─────────────────────────────┬─────────────────────────────┘             │
│                                 │                                            │
└─────────────────────────────────┼────────────────────────────────────────────┘
                                  │
                                  ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                         wasm-node (userspace)                                │
│                                                                              │
│   ┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐         │
│   │  eBPF Loader    │───►│ Event Consumer  │───►│ Action Dispatcher│         │
│   │  (aya-rs)       │    │ (Tokio task)    │    │                  │         │
│   └─────────────────┘    └─────────────────┘    └────────┬────────┘         │
│                                                          │                  │
│                              ┌───────────────────────────┼─────────────────┐│
│                              ▼                           ▼                 ▼│
│                       ┌──────────┐                ┌──────────┐      ┌──────────┐
│                       │ Prometheus│               │   Logs   │      │  NATS    │
│                       │ Metrics  │                │ (audit)  │      │  Events  │
│                       └──────────┘                └──────────┘      └──────────┘
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Key Components

| Component | Technology | Purpose |
|-----------|-----------|---------|
| eBPF Programs | aya-rs (Rust → BPF bytecode) | Kernel-level event hooks |
| Ring Buffer | Linux perf buffer | Lock-free kernel→userspace communication |
| Loader | `aya::Ebpf::load()` | Load and attach programs at runtime |
| Consumer | Tokio background task | Read events, parse, dispatch |
| Action Dispatcher | Rust async | Execute recovery actions |

---

## eBPF Programs

The platform loads six eBPF programs, each monitoring a different subsystem. All programs share a single ring buffer and a configuration map.

### 1. Process Tracker

**Hooks**: `sched_process_exec`, `sched_process_exit` tracepoints

**Monitors**:
- Wasm instance thread exits (panic, kill, OOM)
- Unexpected child processes (defense in depth)
- OOM kills by signal detection (`SIGKILL` with exit code 0)

**Event**: `ProcessEvent`

**Actions**:
- **OOM kill**: Notify Supervisor immediately, emit metric, log SECURITY alert
- **Normal exit**: Preemptively remove from upstream table (no 502s during health loop gap)
- **Signal death**: Log signal number, notify Supervisor

**Prometheus metrics**:
```
wasm_ebpf_oom_kills_total{app_id="api-users"}
wasm_ebpf_process_exits_total{app_id="api-users",reason="oom|signal|normal"}
```

### 2. TCP Connection Monitor

**Hooks**: `inet_sock_set_state` tracepoint

**Monitors**:
- TCP connection state transitions per PID
- Connection count per instance
- Retransmit detection (earliest network degradation signal)
- Connection storm detection (burst of `SYN_SENT`)

**Event**: `TcpEvent`

**Actions**:
- **Connection limit exceeded**: Activate backpressure before userspace rate limiter sees the request
- **Retransmit spike on NATS port (4222)**: Preemptively mark NATS degraded, start catch-up early
- **Connection storm**: Activate Slowloris protection (reduce timeouts, lower per-IP limits)

**Prometheus metrics**:
```
wasm_ebpf_tcp_retransmits_total{node_id="node-1"}
wasm_ebpf_tcp_connections{pid="12345"}
```

### 3. File Descriptor Watcher

**Hooks**: `fd_install` (open), `do_filp_close` (close) kprobes

**Monitors**:
- FD count per PID in real time
- FD limit approach (80% soft limit)
- FD leak detection (monotonic increase over 3 windows)

**Event**: `FdEvent`

**Actions**:
- **Soft limit (80%)**: Log warning, emit gauge, consider pruning idle instances
- **Hard limit (95%)**: Kill most idle instance, activate backpressure until FDs freed
- **FD leak**: Log SECURITY alert, kill instance, audit log entry

**Prometheus metrics**:
```
wasm_ebpf_fd_usage_ratio{pid="12345"}
wasm_ebpf_fd_limit_approaching_total{pid="12345"}
```

### 4. Memory Pressure Sentinel

**Hooks**: `try_to_free_pages` kprobe, `vmpressure_level_change` tracepoint

**Monitors**:
- `kswapd` background reclaim activity
- Direct reclaim (allocation path under pressure)
- `vmpressure` notifier levels: low, medium, critical
- Anonymous page tracking (Wasm linear memory)

**Event**: `MemPressureEvent`

**Graduated Response**:

| Level | Trigger | Action |
|-------|---------|--------|
| **Low** | `kswapd` active | Log info. Emit `memory_pressure=1`. No instance action. |
| **Medium** | Direct reclaim | Log warning. Emit `memory_pressure=2`. Prune idle instances. Backpressure 30s. No cold starts. |
| **Critical** | vmpressure critical | Log error. Emit `memory_pressure=3`. Kill largest instance. Kill non-essential instances. Permanent backpressure. Publish `NodeUnderPressure` to NATS. |

This graduated response **prevents the OOM killer from ever firing**.

**Prometheus metrics**:
```
wasm_ebpf_memory_pressure{node_id="node-1"}  # 0=none, 1=low, 2=medium, 3=critical
```

### 5. Disk I/O Monitor

**Hooks**: `block_rq_issue`, `block_rq_complete` tracepoints

**Monitors**:
- I/O latency per block device (issue → complete time)
- Slow I/O detection (threshold: 50ms default)
- Write amplification tracking

**Event**: `DiskIoEvent`

**Actions**:
- **Slow I/O**: Log warning, emit histogram metric. If device holds `state.redb`, switch to read-only temporarily.
- **Sustained slow I/O (>30s)**: Enter degraded mode. Publish `NodeUnderPressure`. Other nodes stop steering traffic.
- **Recovered**: Exit degraded mode. Publish `NodeReady`.

**Prometheus metrics**:
```
wasm_ebpf_disk_io_latency_seconds_bucket{dev="sda"}
wasm_ebpf_disk_slow_io_total{dev="sda"}
```

### 6. Syscall Anomaly Detector

**Hooks**: `raw_syscalls/sys_enter` tracepoint

**Monitors**:
- Syscall rate per PID (detect tight syscall loops)
- Privileged syscalls (`ptrace`, `bpf`, `mount`, `setuid`)
- Unexpected `execve` from Wasm instances
- Network control syscalls (`bind` on unauthorized ports)

**Event**: `SyscallEvent`

**Actions**:
- **Privilege escalation**: **Critical security incident**. Kill instance immediately. Log SECURITY alert. Emit metric. Publish `SecurityIncident` to NATS. Quarantine artifact hash.
- **High syscall rate**: Reduce fuel allocation or kill if persistent.
- **`execve` from Wasm**: Kill instance, SECURITY alert (indicates Wasmtime bug or compromise).

**Prometheus metrics**:
```
wasm_ebpf_security_violations_total{node_id="node-1",syscall="ptrace"}
wasm_ebpf_syscall_rate{pid="12345"}
```

---

## Configuration

### Config File

```toml
[ebpf]
enabled = true
fd_soft_limit = 8192          # Warn at 80% of 10240
fd_hard_limit = 9728          # Kill at 95% of 10240
mem_low_threshold_pages = 65536      # ~256 MB free
mem_critical_threshold_pages = 16384 # ~64 MB free
disk_slow_threshold_ns = 50000000    # 50 ms
tcp_conn_limit_per_pid = 10000
syscall_rate_limit = 100000   # per second
sampling_period_secs = 10

# Enable/disable individual programs
enable_process_tracker = true
enable_tcp_monitor = true
enable_fd_watcher = true
enable_mem_pressure = true
enable_disk_monitor = true
enable_syscall_counter = true
```

### Environment Variables

```bash
WASM_NODE_EBPF_ENABLED=true
WASM_NODE_EBPF_FD_SOFT_LIMIT=8192
WASM_NODE_EBPF_MEM_CRITICAL_THRESHOLD_PAGES=16384
WASM_NODE_EBPF_DISK_SLOW_THRESHOLD_NS=50000000
```

### Hot-Reloadable Parameters

All eBPF thresholds are hot-reloadable via `wasm-ctl` or the admin API:

```bash
# View current eBPF config
wasm-ctl node config | grep ebpf

# Update threshold at runtime
wasm-ctl node config --set ebpf_fd_soft_limit=4096
wasm-ctl node config --set ebpf_disk_slow_threshold_ns=100000000

# Disable a specific program
wasm-ctl node config --set ebpf_enable_syscall_counter=false
```

The updated config is written to the eBPF config map within 1 second. No restart required.

---

## Operational Commands

### Check eBPF Status

```bash
wasm-ctl node ebpf-status
```

Example output:
```
Mode:                    eBPF (kernel)
Kernel:                  6.8.0-generic
BTF:                     available
Programs loaded:         6/6
Ring buffer size:        1 MB
Events processed:        1048576
Events dropped:          0
Parse errors:            0

Backpressure:            normal
Degraded mode:           no
Pressure level:          none

OOM kills (total):       0
Process exits (total):   12
TCP retransmits:         3
Security violations:     0
FD limit approaches:     1
Disk slow I/O events:    0
Memory pressure events:  2 (low)
```

### Manual Recovery Commands

```bash
# Prune idle instances to free FDs
wasm-ctl node ebpf-config --prune-idle --idle-threshold-secs 60

# Kill the largest instance (memory pressure recovery)
wasm-ctl node ebpf-config --kill-largest --reason "manual memory pressure recovery"

# Reset backpressure to normal
wasm-ctl node ebpf-config --clear-backpressure

# Trigger a full eBPF program reload (if maps are corrupted)
wasm-ctl node ebpf-config --reload
```

### View Recent Security Incidents

```bash
# Last 20 security events
grep "SECURITY" /var/log/wasm-node/audit.jsonl | tail -20

# Or via NATS event stream
wasm-ctl events --subject "security.incidents.>" --last 50
```

### Metrics Queries

```bash
# Current memory pressure by node
curl -s http://node:9090/metrics | grep wasm_ebpf_memory_pressure

# Security violations in last 5 minutes
curl -s http://prometheus:9090/api/v1/query \
  -d 'query=rate(wasm_ebpf_security_violations_total[5m])'

# FD usage ratio by PID
curl -s http://node:9090/metrics | grep wasm_ebpf_fd_usage_ratio
```

---

## Security Incident Response

### Severity Levels

| Level | Event | Auto-Action | Operator Action |
|-------|-------|-------------|-----------------|
| **Critical** | Privilege escalation syscall (`ptrace`, `bpf`, `mount`) | Kill instance, quarantine hash, publish `SecurityIncident` | Investigate immediately. Check if false positive. Update WASI policy if needed. |
| **Critical** | `execve` from Wasm instance | Kill instance, quarantine hash | Investigate. Likely Wasmtime bug or compromised host. |
| **High** | FD leak (monotonic increase) | Kill instance, audit log | Review app code. Check for missing `close()` in WASI bindings. |
| **High** | Memory pressure critical | Kill largest instance, backpressure | Add RAM or reduce instance density. |
| **Medium** | Syscall rate limit exceeded | Throttle fuel or kill | Check for infinite loops in app. |
| **Medium** | TCP retransmit spike | Mark NATS degraded | Check network path to NATS. |
| **Low** | FD soft limit approach | Log, emit metric | Monitor. Prune idle instances if sustained. |

### Incident Response Playbook

**Step 1: Identify**
```bash
wasm-ctl node ebpf-status
# Check security_violations count
```

**Step 2: Isolate**
```bash
# The system already killed the instance and quarantined the hash
# Verify the app is not running
wasm-ctl instances --app <app_id>
```

**Step 3: Investigate**
```bash
# Get incident details from audit log
jq 'select(.event_type == "SyscallAnomaly")' /var/log/wasm-node/audit.jsonl | tail -5

# Check if it was a false positive
grep "SyscallAnomaly" /var/log/wasm-node/app.log | grep <pid>
```

**Step 4: Remediate**
- If false positive: Update WASI policy allowlist in config, reload
- If real: Do not redeploy same artifact hash. Patch app. Deploy new version.

**Step 5: Verify**
```bash
wasm-ctl node ebpf-status
# Confirm security_violations count stable
```

---

## Monitoring eBPF Itself

### eBPF Health Metrics

The eBPF system exports its own health metrics:

```
wasm_ebpf_programs_loaded{program="process_tracker"} 1
wasm_ebpf_programs_loaded{program="tcp_monitor"} 1
wasm_ebpf_events_processed_total 1048576
wasm_ebpf_events_dropped_total 0
wasm_ebpf_parse_errors_total 0
wasm_ebpf_ring_buffer_full_total 0
```

### Alerts for eBPF System Health

```yaml
# prometheus/alerts.yml
- alert: EbpfProgramsNotLoaded
  expr: sum(wasm_ebpf_programs_loaded) < 6
  for: 1m
  labels:
    severity: warning
  annotations:
    summary: "eBPF programs not fully loaded on {{ $labels.node_id }}"

- alert: EbpfEventsDropped
  expr: rate(wasm_ebpf_events_dropped_total[5m]) > 0
  for: 1m
  labels:
    severity: warning
  annotations:
    summary: "eBPF ring buffer dropping events on {{ $labels.node_id }}"
    description: "Increase ring buffer size or reduce event generation."

- alert: EbpfParseErrors
  expr: rate(wasm_ebpf_parse_errors_total[5m]) > 0
  for: 1m
  labels:
    severity: warning
  annotations:
    summary: "eBPF event parsing errors on {{ $labels.node_id }}"
    description: "Kernel/userspace struct mismatch. May need platform upgrade."
```

---

## Troubleshooting

### Symptom: eBPF programs fail to load

**Check:**
```bash
# Kernel version (requires 5.8+)
uname -r

# BTF availability
ls /sys/kernel/btf/vmlinux

# Capabilities
capsh --print | grep cap_bpf

# dmesg for verifier errors
dmesg | tail -50 | grep "bpf"
```

**Fix:**
- Kernel < 5.8: eBPF disabled, fallback to userspace polling
- Missing BTF: Build kernel with `CONFIG_DEBUG_INFO_BTF=y`
- Missing `CAP_BPF`: Run as root or add capability: `setcap cap_bpf,cap_perfmon,cap_net_admin+ep ./wasm-node`
- Verifier error: Check struct alignment between kernel and userspace. Upgrade platform.

### Symptom: Ring buffer dropping events

**Check:**
```bash
wasm-ctl node ebpf-status
# Look for "Events dropped"
```

**Fix:**
- Heavy load: Increase ring buffer size (requires restart)
- Reduce sampling: Increase `sampling_period_secs`
- Disable less critical programs: Set `enable_disk_monitor = false`

### Symptom: False positive security violations

**Check:**
```bash
# Which syscall triggered?
grep "SyscallAnomaly" /var/log/wasm-node/audit.jsonl | jq '.syscall_nr'

# Map syscall number to name
cat /usr/include/asm/unistd_64.h | grep <nr>
```

**Fix:**
- Legitimate syscall: Update WASI policy allowlist in node config
- Hot-reload config: `wasm-ctl node config --set ...`
- If persistent: Disable syscall counter temporarily

### Symptom: eBPF causing high CPU

**Check:**
```bash
# eBPF CPU usage per program
bpftool prog show | grep -E "name|run_time"

# Overall eBPF overhead
cat /proc/vmstat | grep -i bpf
```

**Fix:**
- Reduce sampling frequency: `sampling_period_secs = 30`
- Disable heavy programs (disk monitor on high-I/O nodes)
- Ensure BTF is available (reduces program complexity)

---

## Non-Linux Fallback

On macOS, Windows, or Linux kernels < 4.15, eBPF is not available. The platform degrades gracefully:

```
┌─────────────────────────────────────────────────────────────┐
│              Non-Linux / Old Kernel Host                     │
│                                                              │
│   ┌─────────────────┐    ┌─────────────────┐                │
│   │ Userspace Polling│───►│ Existing Health │                │
│   │ (5s interval)   │    │ Loop (Step 07)  │                │
│   └─────────────────┘    └─────────────────┘                │
│                                                              │
│   Detection latency: seconds instead of milliseconds         │
│   No syscall anomaly detection                               │
│   No preemptive memory pressure handling                     │
│   All other platform features work normally                  │
└─────────────────────────────────────────────────────────────┘
```

### Fallback Behavior

| Feature | eBPF Mode | Fallback Mode |
|---------|-----------|---------------|
| Instance crash detection | <1ms | 0–5s (health loop) |
| Memory pressure | Preemptive (3 levels) | Reactive (OOM kill) |
| FD exhaustion | Pre-warning | At failure time |
| Syscall anomaly | Detected | Not detected |
| Disk I/O saturation | ~1ms | Prometheus alert (minutes) |
| TCP retransmit prediction | <1ms | Heartbeat timeout (seconds) |

The platform is **fully functional** without eBPF. eBPF is an enhancement, not a dependency.

---

## Performance Impact

### Benchmarks

Measured on a 16-core AMD EPYC, kernel 6.8, 10,000 requests/sec:

| Metric | Without eBPF | With eBPF | Overhead |
|--------|--------------|-----------|----------|
| Request latency (p99) | 12ms | 12.1ms | +0.8% |
| CPU usage (node) | 45% | 47% | +4.4% |
| Memory usage | 2.1GB | 2.15GB | +2.4% |
| eBPF ring buffer throughput | — | 50,000 events/sec | — |

### When eBPF Overhead is Noticeable

- **Very high syscall rates** (>100k/sec per PID): syscall counter adds ~1μs per syscall
- **High churn workloads** (thousands of spawn/kill per minute): process tracker fires frequently
- **Old kernels without BTF**: programs are less optimized, higher overhead

### Mitigations

- Increase `sampling_period_secs` to reduce periodic work
- Disable individual programs if not needed
- Use `perf_event_open` instead of tracepoints where available (lower overhead)

---

## Quick Reference

| Task | Command |
|------|---------|
| Check eBPF status | `wasm-ctl node ebpf-status` |
| View eBPF config | `wasm-ctl node config \| grep ebpf` |
| Update threshold | `wasm-ctl node config --set ebpf_fd_soft_limit=4096` |
| Prune idle instances | `wasm-ctl node ebpf-config --prune-idle` |
| Kill largest instance | `wasm-ctl node ebpf-config --kill-largest` |
| Reload eBPF programs | `wasm-ctl node ebpf-config --reload` |
| View security incidents | `grep SECURITY /var/log/wasm-node/audit.jsonl` |
| Check metrics | `curl http://node:9090/metrics \| grep wasm_ebpf` |
| Disable eBPF | `wasm-ctl node config --set ebpf_enabled=false` |
| Enable specific program | `wasm-ctl node config --set ebpf_enable_mem_pressure=true` |

---

## Further Reading

- Implementation spec: `INFRA_IMPL/30_EBPF_MONITORING_RECOVERY.md`
- Metrics & alerting: `docs/observability.md`
- Security architecture: `INFRA_IMPL/13_SECURITY.md`
- aya framework: https://aya-rs.dev/
- eBPF verifier internals: https://docs.kernel.org/bpf/verifier.html

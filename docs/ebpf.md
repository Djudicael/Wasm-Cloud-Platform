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
- **Verifier checked**: The kernel verifier rejects many unsafe programs before attachment; eBPF still requires careful review and host-kernel patching

On this platform, eBPF provides event-driven kernel signals. Detection latency depends on the hook, scheduler load, ring-buffer delivery, and userspace dispatch; the platform does not claim a universal sub-millisecond service level.

---

## Why eBPF on This Platform

### Detection and attribution

| Signal | Kernel mode | Userspace fallback |
|---------|-------------|--------------------|
| Process lifecycle | Tracepoint/syscall events for registered runtime TIDs and the node PID | Supervisor task and health-loop state |
| TCP activity | TID-scoped connection events with port correlation | Socket and dependency health probes |
| File descriptors | TID-scoped open/install/close activity | Process-level polling |
| Memory pressure | Events restricted to the dedicated `wasm-node` cgroup | `/proc/meminfo` polling |
| Block I/O | Issue/completion records originating in the node cgroup | No equivalent application-attributed signal |
| Syscalls | Counts for registered runtime TIDs | No equivalent syscall stream |
| Namespace policy | Socket and namespace-related events for registered TIDs | Runtime socket policy remains authoritative |

These signals shorten some detection paths and improve attribution, but they do not replace the supervisor, WASI policy enforcement, readiness checks, Prometheus alerts, or host monitoring.

### What eBPF enables

1. Earlier resource-pressure and lifecycle signals for the supervisor and proxy.
2. TID and node-cgroup attribution for events that retain application execution context.
3. Metrics for monitor availability, parser failures, queue saturation, and ring-buffer drops.
4. Node-local namespace and forged-header defense in depth.
5. A required-monitoring mode that fails readiness when kernel monitoring cannot remain active.

The current in-process runtime is a single trust domain. Applications share the `wasm-node` process, UID, address space, capabilities, and node cgroup; eBPF does not turn that design into hostile multi-tenant isolation.

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
| Ring buffers | aya ring buffers, one set per loaded object | Kernel-to-userspace event delivery |
| Loader | `aya::Ebpf::load()` | Load and attach programs at runtime |
| Consumer | Tokio background task | Read events, parse, dispatch |
| Action Dispatcher | Rust async | Translate events into metrics, backpressure, cleanup callbacks, and control-plane events |

---

## eBPF Programs

The build produces seven independent eBPF objects. Each object owns its program links, CONFIG map, monitoring identity maps, ring buffer, and drop counters for the lifetime of the monitor handle.

| Object | Main scope | Platform use |
|--------|------------|--------------|
| `process_tracker` | Raw syscall entry plus process exec/exit for registered runtime TIDs and the node PID | Lifecycle counters and exit/OOM signals |
| `tcp_monitor` | TCP state/send/receive hooks for registered runtime TIDs | Connection counts, retransmit signals, and port-to-TID close correlation |
| `fd_watcher` | File open/install/close activity for registered runtime TIDs | FD counts and threshold actions |
| `mem_pressure` | Direct reclaim activity restricted to the dedicated node cgroup | Pressure level, idle pruning, and backpressure |
| `disk_monitor` | Block request issue/completion originating from the node cgroup | Latency and completed-byte metrics |
| `syscall_counter` | Raw syscall entry for registered runtime TIDs | Syscall-rate and selected security signals |
| `namespace_enforcer` | Socket and namespace-related hooks for registered runtime TIDs | Node-local mesh identity and forged-header defense in depth |

### 1. Process tracker

The process tracker observes raw syscall entry plus process execution and exit events. It accepts events for registered WASI runtime TIDs and for the node PID, and carries the registered application identity when the event occurs in a runtime worker.

The userspace dispatcher uses these records for lifecycle, exit, and OOM-related counters and callbacks. A process event is an early signal for reconciliation; the supervisor task result and health state remain the source used to decide whether capacity must be replaced. The monitor does not claim that every `SIGKILL` is an OOM kill without supporting evidence.

### 2. TCP monitor

The TCP object observes state, send, and receive hooks for registered runtime TIDs. It records connection activity and retransmit-related signals and maintains port-to-TID correlation so a close event can release the correct runtime connection reservation.

This correlation is especially relevant for long-lived WASI CLI workloads: the close may arrive outside the immediate request path. The dispatcher can update pressure and connectivity metrics, but NATS health is also checked at the protocol layer; a retransmit alone is not treated as proof that JetStream is unavailable.

### 3. File-descriptor watcher

The FD object observes file open, descriptor installation, and close activity for registered runtime TIDs. Its soft and hard thresholds come from the eBPF configuration and can trigger warning, pruning, or backpressure behavior through the userspace dispatcher.

The WASI `ResourceTable` limit remains the guest-facing descriptor bound. Kernel FD events add visibility and recovery signals around the whole node process; they are not a substitute for the runtime limit and do not provide byte-level filesystem accounting.

### 4. Memory-pressure monitor

The memory object observes direct-reclaim activity and restricts accepted events to the dedicated `wasm-node` cgroup. Configured low and critical page thresholds feed pressure levels into the dispatcher.

At higher pressure the dispatcher can stop new work, prune idle instances, and invoke the configured largest-instance termination callback. These actions reduce risk but cannot guarantee that the host OOM killer never runs. Operators must still configure host and cgroup memory limits and alert on sustained pressure.

### 5. Disk-I/O monitor

The disk object correlates block request issue and completion records that originate from the node cgroup. It exports completed operation, byte, and latency information and identifies operations over `disk_slow_threshold_ns`.

Block tracepoints describe kernel requests rather than WASI file operations. Direct I/O can retain useful node/application context, while buffered ext4 writeback may later run in a kernel worker. The monitor deliberately avoids assigning that work to an arbitrary application, so its byte totals must not be used as authoritative per-tenant billing or quota data.

### 6. Syscall counter

The syscall object counts raw syscall entry for registered runtime TIDs and reports selected security-sensitive activity and configured rate pressure. The dispatcher can emit metrics, audit/control-plane events, and a workload response when its classification rules match.

These events are defense-in-depth evidence. Wasmtime and WASI capability policy remain the application sandbox boundary, and operators should correlate the syscall number, TID, application registration, surrounding audit events, and artifact provenance before classifying an incident.

### 7. Namespace enforcer

The namespace object observes socket and namespace-related activity for registered runtime TIDs. Together with the userspace namespace map, it supports source-port-to-application attribution for requests arriving at the loopback internal gateway and detects selected attempts to forge internal identity headers.

The internal gateway strips caller-supplied identity headers and fails closed when required attribution is unavailable. Cross-namespace calls require an explicit allowlist. The object does not provide cross-host identity or forwarding; `<app>.<namespace>.internal` deliberately resolves to the node-local gateway.

### Program and resource metrics

The event dispatcher exports these workload and resource series in addition to the monitor-health series listed later in this guide:

| Metric | Meaning |
|--------|---------|
| `wasm_ebpf_oom_kills_total` | OOM-classified process events by application |
| `wasm_ebpf_process_exits_total` | Process exits by application and classification |
| `wasm_ebpf_signal_deaths_total` | Signal-related deaths by application |
| `wasm_ebpf_tcp_retransmits_total` | TCP retransmit observations |
| `wasm_ebpf_nats_retransmits_total` | Retransmits associated with the NATS path |
| `wasm_ebpf_tcp_connection_count` | Current tracked TCP connections |
| `wasm_ebpf_fd_count` | Current tracked descriptor count |
| `wasm_ebpf_fd_usage_ratio` | Descriptor use relative to the configured threshold |
| `wasm_ebpf_memory_pressure_level` | Current dispatcher pressure level |
| `wasm_ebpf_disk_io_latency_seconds` | Completed block-request latency histogram |
| `wasm_ebpf_disk_io_bytes_total` | Completed bytes by operation/device labels |
| `wasm_ebpf_security_violations_total` | Classified syscall or namespace security events |

Label sets are defined by `crates/ebpf-monitor/src/metrics.rs`. Keep PromQL and alert labels synchronized with that registry when metrics change.

### Identity scope

WASI CLI execution uses a dedicated runtime thread. Its TID is registered in every relevant MONITORED_TIDS map. The loader also records the cgroup-v2 ID of the `wasm-node` service. Events outside those identities are discarded where the probe supports that filter.

Buffered filesystem writeback can run in a kernel worker after application context is lost. The disk monitor excludes or reports such work without claiming exact per-application ownership. Byte-accurate filesystem and egress quotas remain runtime or host-wrapper work.

### Event handling and recovery

The userspace dispatcher updates metrics and can invoke supervisor callbacks for idle pruning, largest-instance termination, and backpressure. Recovery actions depend on the event type and configured threshold; an observed kernel event does not by itself prove compromise.

Security incidents are published through the normal NATS control path when the dispatcher classifies a matching event. Operators must correlate the application ID, TID/cgroup identity, audit record, node logs, and workload behavior before deciding whether an artifact is malicious.

### Lifecycle

`LoadedEbpf` owns all programs, maps, and links. Dropping the monitor detaches them. The lifecycle validation script repeatedly deploys, restarts, and removes workloads and verifies that stale application identities are not left in the maps.

---

## Configuration

### Config file

```toml
[runtime]
isolation_mode = "single-trust-domain"

[ebpf]
enabled = true
required = true
fd_soft_limit = 8192
fd_hard_limit = 9728
mem_low_threshold_pages = 65536
mem_critical_threshold_pages = 16384
disk_slow_threshold_ns = 50000000
tcp_conn_limit_per_pid = 10000
syscall_rate_limit = 100000
sampling_period_secs = 10
enable_namespace_enforcer = true
gateway_port = 9080
enable_forged_header_detect = true
```

Production admission requires `runtime.isolation_mode = "single-trust-domain"` and `ebpf.required = true`. Optional mode starts the reduced userspace fallback and reports degraded monitoring when loading, attachment, a probe, or the consumer fails.

### Environment variables

The normal `WASM_NODE_<SECTION>_<KEY>` mapping applies to supported configuration fields, for example:

```bash
WASM_NODE_EBPF_ENABLED=true
WASM_NODE_EBPF_REQUIRED=true
WASM_NODE_EBPF_MEM_CRITICAL_THRESHOLD_PAGES=16384
WASM_NODE_EBPF_DISK_SLOW_THRESHOLD_NS=50000000
```

### Hot-reloadable thresholds

The supported hot-config keys use dotted names:

```bash
wasm-ctl node config --json | jq '.ebpf'
wasm-ctl node config \
  --set ebpf.mem_low_threshold_pages=65536 \
  --set ebpf.mem_critical_threshold_pages=16384 \
  --set ebpf.disk_slow_threshold_ns=50000000
```

The node updates the userspace dispatcher and every loaded kernel CONFIG map. Enabling or disabling eBPF, changing required mode, and changing individual program selection are startup decisions and require a controlled node restart.

---

## Operational Commands

### Check eBPF status

```bash
wasm-ctl node ebpf-status
```

The response reports whether eBPF is active, required, or degraded; the number of attached programs; the degradation reason; queue/backpressure state; event and parser counters; and resource/security totals.

### Manual recovery commands

```bash
# Prune instances idle for at least 60 seconds
wasm-ctl node ebpf-config --prune-idle --idle-threshold-secs 60

# Terminate the instance currently reporting the largest memory footprint
wasm-ctl node ebpf-config \
  --kill-largest \
  --kill-largest-reason "manual memory pressure recovery"
```

The CLI does not expose eBPF reload or clear-backpressure actions. Program reload requires a controlled node restart. Backpressure clears through the dispatcher's recovery logic.

### Metrics queries

```bash
curl -s http://node:9090/metrics | grep '^wasm_ebpf_'
curl -s http://prometheus:9090/api/v1/query \
  --data-urlencode 'query=rate(wasm_ebpf_security_violations_total[5m])'
```

Security incidents are available in the configured audit/log pipeline and NATS subject permissions. There is no top-level `wasm-ctl events` command.

---

## Security Incident Response

### Severity and evidence

| Signal | Automatic platform response | Operator check |
|--------|-----------------------------|----------------|
| Process exit or OOM classification | Counters, audit/log signal, and configured backpressure callback | Confirm task exit, billing finalization, and replacement capacity |
| FD threshold | Metric update; hard-threshold handling can prune idle instances and enable backpressure | Inspect node FD use and workload lifecycle |
| Memory pressure | Pressure metric; medium/critical handling can prune idle instances and enable backpressure | Check host/cgroup memory, recent deployments, and sustained pressure |
| TCP retransmit or connection pressure | Metrics and degraded/backpressure signals according to dispatcher logic | Check NATS and application network paths |
| Syscall or forged-header violation | Security metric, audit/control-plane event, and configured workload response | Correlate TID, application identity, policy, and artifact provenance |

### Incident response playbook

1. Record `wasm-ctl node ebpf-status`, readiness, relevant Prometheus series, and the node's structured logs.
2. Confirm that the reported TID and cgroup belong to the expected `wasm-node` process and application.
3. Fence or remove the application through the normal deployment lifecycle when containment is required.
4. Preserve the artifact digest, deployment manifest, audit records, and release provenance.
5. Reproduce on an isolated node class before changing thresholds or WASI policy.
6. Deploy a new signed artifact version and verify that counters stabilize.

Do not assume that the monitor quarantines every suspicious artifact automatically. Containment and artifact admission remain separate controls.

---

## Monitoring eBPF Itself

The monitor exports availability and data-path health separately from application health:

```text
wasm_ebpf_active
wasm_ebpf_monitoring_required
wasm_ebpf_monitoring_degraded
wasm_ebpf_monitoring_failures_total{reason="..."}
wasm_ebpf_events_processed_total
wasm_ebpf_events_by_type_total{event_type="..."}
wasm_ebpf_events_parse_errors_total
wasm_ebpf_ring_buffer_dropped_events_total{program="..."}
wasm_ebpf_ring_buffer_drop_counter_read_errors_total{program="..."}
wasm_ebpf_dispatch_queue_depth
wasm_ebpf_dispatch_queue_capacity
wasm_ebpf_dispatch_queue_saturations_total
```

The tracked rules in `config/prometheus/rules/wasm-cloud-platform.yml` distinguish monitor unavailability from a down node. Validate them and the state-scoped Alertmanager delivery path with:

```bash
bash scripts/vm/validate-alerting.sh --state-file PATH
```

A zero event count is not sufficient proof of monitor health; alert on the availability, drop, parse-error, and queue-saturation series as well.

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
- If the drop source remains saturated, change startup probe selection only through a reviewed code/config change and restart; the current operator schema does not expose a per-program disk-monitor toggle.

### Symptom: False positive security violations

**Check:**
```bash
# Which syscall triggered?
grep "SyscallAnomaly" /var/log/wasm-node/audit.jsonl | jq '.syscall_nr'

# Map syscall number to name
cat /usr/include/asm/unistd_64.h | grep <nr>
```

**Fix:**
- Legitimate workload activity: verify the event attribution and adjust the WASI policy through a reviewed deployment manifest when that policy actually governs the operation.
- Threshold-only changes: use the three documented `ebpf.*` hot-config keys.
- Persistent unexplained events: fence the workload and reproduce on an isolated node; the current operator schema does not expose a live syscall-counter toggle.

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
- If a specific probe dominates cost, change startup probe selection through a reviewed configuration/code change and restart the node
- Ensure BTF is available (reduces program complexity)

---

## Non-Linux Fallback

When eBPF is disabled or optional and unavailable, the monitor starts a userspace polling fallback. It provides process-level memory/FD/TCP observations and feeds the same dispatcher where supported.

| Capability | Kernel mode | Userspace fallback |
|------------|-------------|--------------------|
| TID/cgroup application attribution | Available for supported probes | Unavailable |
| Namespace/forged-header kernel signals | Available | Unavailable |
| Block request latency/bytes | Available within the documented attribution limit | Unavailable |
| Process-level pressure sampling | Available alongside events | Available |
| Required-monitoring readiness | Passes only while kernel monitoring is active | Fails |

The fallback is suitable only where reduced monitoring is explicitly accepted. Production configuration requires eBPF because node-local mesh identity and persistent WASI CLI connection cleanup depend on its kernel events.

---

## Performance Impact

The repository does not claim a universal latency or CPU overhead. Cost varies with kernel, enabled hooks, event rate, workload syscall/network behavior, and node size.

Use the controlled validation script to compare a baseline and monitored run on the intended node class:

```bash
bash scripts/vm/validate-ebpf-overhead.sh \
  --state-file PATH \
  --evidence-dir EVIDENCE_DIRECTORY
```

Preserve the raw samples and result summary with the production-validation evidence. A Firecracker or WSL result characterizes that test environment only; repeat the gate on every production host class.

To reduce overhead, first identify the busy event type and ring-buffer source from metrics. Changing startup program selection requires a restart, and weakening required monitoring changes the production security contract.

---

## Quick Reference

| Task | Command |
|------|---------|
| Check eBPF status | `wasm-ctl node ebpf-status` |
| View eBPF config | `wasm-ctl node config \| grep ebpf` |
| Update thresholds | `wasm-ctl node config --set ebpf.mem_low_threshold_pages=65536` |
| Prune idle instances | `wasm-ctl node ebpf-config --prune-idle` |
| Kill largest instance | `wasm-ctl node ebpf-config --kill-largest` |
| View security incidents | `grep SECURITY /var/log/wasm-node/audit.jsonl` |
| Check metrics | `curl http://node:9090/metrics \| grep wasm_ebpf` |

---

## Further Reading

- Implementation spec: `INFRA_IMPL/30_EBPF_MONITORING_RECOVERY.md`
- Metrics & alerting: `docs/observability.md`
- Security architecture: `INFRA_IMPL/13_SECURITY.md`
- aya framework: https://aya-rs.dev/
- eBPF verifier internals: https://docs.kernel.org/bpf/verifier.html

# ebpf-monitor

`ebpf-monitor` supplies Linux kernel observability and selected recovery signals
for `wasm-node`. It loads seven independently compiled eBPF objects when the
`ebpf` feature is enabled and uses a reduced userspace fallback when kernel
monitoring is optional and unavailable.

## Production boundary

The current runtime is an in-process WASI host. Each application gets a
dedicated single-thread Tokio runtime and its worker TID is registered in the
eBPF `MONITORED_TIDS` maps. This supports node-local application attribution for
events produced in that worker context. It is not a hostile multi-tenant
security boundary: all applications still share the `wasm-node` address space,
UID, process capabilities, and node cgroup.

Production configuration therefore admits only:

```toml
[runtime]
isolation_mode = "single-trust-domain"

[ebpf]
enabled = true
required = true
```

Configuration validation rejects any other isolation-mode claim. Operators
must place each `wasm-node` process in a dedicated cgroup-v2 cgroup. The loader
resolves that cgroup's kernel ID and writes it to every BPF `CONFIG` map.
Block-I/O and memory-pressure probes discard events originating outside that
cgroup. TID-filtered TCP, file-descriptor, syscall, namespace, and process
events retain their application identity.

Buffered writeback may execute in a kernel worker rather than the application
worker's TID/cgroup context. It is intentionally not advertised as exact
per-application block-I/O attribution. Use process-per-application execution,
separate cgroups, and a fresh validation campaign before allowing mutually
untrusted tenants on one node.

## Programs and scope

| Object | Main hooks | Event scope |
|---|---|---|
| `process_tracker` | raw syscall entry, process exec/exit | registered WASI runtime TIDs and node PID |
| `tcp_monitor` | TCP state/send/receive hooks | registered WASI runtime TIDs, with port-to-TID close correlation |
| `fd_watcher` | open/install/close hooks | registered WASI runtime TIDs |
| `mem_pressure` | direct-reclaim hooks | dedicated `wasm-node` cgroup |
| `disk_monitor` | block request issue/complete | requests issued from the dedicated `wasm-node` cgroup |
| `syscall_counter` | raw syscall entry | registered WASI runtime TIDs |
| `namespace_enforcer` | socket and namespace-related syscall hooks | registered WASI runtime TIDs |

Each object has its own `CONFIG` and ring-buffer maps. `LoadedEbpf` owns all
programs, maps, and attachment links for the node lifetime. Dropping the monitor
handle detaches them; lifecycle validation checks that repeated deploy/restart/
remove operations do not leave stale application identities.

## Availability behavior

- `ebpf.enabled = false`: kernel monitoring is intentionally disabled.
- enabled and optional: an unsupported kernel, missing privileges/BTF, rejected
  program, unavailable probe, or terminated consumer is reported as degraded
  and the supported userspace fallback starts.
- enabled and required: the same condition makes readiness fail. Production
  admission requires this mode because persistent WASI CLI connection cleanup
  and node-local mesh identity rely on kernel events.

`/admin/ebpf/status` and Prometheus metrics distinguish application health from
monitor availability. Alerts must likewise distinguish
`EbpfMonitoringUnavailable` from `PlatformNodeDown`.

## Build and validation

Run in Linux or WSL2:

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target
bash scripts/ebpf/build-ebpf.sh
cargo test -p ebpf-monitor --lib
```

The BPF build uses the pinned nightly in `scripts/ebpf/build-ebpf.sh`, cleans the
isolated BPF target first, and verifies all seven expected objects. A full local
production-contract rehearsal uses:

```bash
bash scripts/vm/validate-workload-isolation-contract.sh \
  --state-file .prod-validation-single-host-state.json \
  --restart-node \
  --evidence-dir INFRA_IMPL/process/prod_validation/evidence/<date>/P10-10-workload-isolation
```

The testbed schema-15 node image mounts cgroup v2 and starts `wasm-node` in
`/sys/fs/cgroup/wasm-node`. The validation correlates the guest-reported cgroup
inode with the ID written to the BPF maps, requires seven active programs and
mandatory monitoring, and records immutable checksums. The Firecracker result
is local evidence, not a substitute for rerunning the gate on each production
node class.

## Remaining limits

- The userspace fallback cannot provide TID/cgroup-accurate enforcement.
- No eBPF result makes byte-accurate WASI filesystem or egress quotas
  authoritative; those remain runtime/host-wrapper design work.
- Block request attribution depends on kernel tracepoint semantics. Buffered
  writeback that loses application context is excluded rather than assigned to
  an arbitrary tenant.
- eBPF loading needs the capabilities and kernel facilities documented in the
  production checklist. Run the process with the smallest viable privilege set
  and isolate it in its own cgroup.
- Cross-host service-mesh identity is out of scope by design; platform internal
  dependencies are node-local.

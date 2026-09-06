# P10-10 workload-isolation evidence

Date: 2026-09-05  
Scope: platform source contract plus one-node Firecracker rollout in the existing
three-node local production-validation topology.

## Result

**PASS for the declared single-trust-domain platform model. Hostile
multi-tenancy remains unsupported and is rejected by production configuration
validation.**

The source now makes the actual boundary explicit:

- production requires `runtime.isolation_mode = "single-trust-domain"`;
- production requires `ebpf.enabled = true` and `ebpf.required = true`;
- any other isolation mode is rejected rather than presented as tenant-grade
  isolation;
- block-I/O and memory-pressure programs filter by the node cgroup-v2 ID;
- application-aware probes continue to filter by the dedicated registered WASI
  runtime TID; and
- buffered kernel writeback is not claimed as exact per-application I/O.

## Live validation

The schema-15 rootfs mounted cgroup v2 and launched `wasm-node` through an exec
wrapper that entered `/sys/fs/cgroup/wasm-node` before Rust/eBPF startup. Node
`local-test-node-0` was rolled while nodes 1 and 2 stayed online.

The guest emitted cgroup ID `21`. Every one of the six threshold `CONFIG` maps
logged `node_cgroup_id=21`; the namespace program remained TID/identity scoped.
The node reported:

- `ebpf_active=true`;
- `attached_programs=7`;
- `monitoring_required=true`;
- `monitoring_degraded=false`;
- zero parse errors; and
- healthy readiness with both OIDC components restored.

The OIDC front-door readiness check returned HTTP 200 with `database=ok` after
the rollout. All three platform nodes returned healthy readiness, and
Prometheus reported `wasm_ebpf_active=1` for all three nodes.

## Commands

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target
bash scripts/vm/build-node-rootfs.sh
bash scripts/vm/validate-workload-isolation-contract.sh \
  --state-file .prod-validation-p10-08-state.json \
  --node-id local-test-node-0 \
  --restart-node \
  --evidence-dir INFRA_IMPL/process/prod_validation/evidence/2026-09-05-single-host/P10-10-workload-isolation
curl -H 'Host: oidc-backend.internal' http://127.0.0.1:8088/health/ready
```

The first validator attempt successfully rolled the node but rejected the
capture because the serial stream ended the marker with CR and the readiness
API uses `healthy`, not `ready`. Those assertions were corrected and the full
validator then passed without a second restart.

Repository validation passed formatting, Clippy with warnings denied, the
native all-targets check, both explicit `wasm32-wasip2` builds, all seven BPF
objects, 90 common tests, 53 configuration tests, 127 feature-enabled eBPF
tests, and the node suites (33 library, 83 binary, 8 cluster-bootstrap, and 2
database integration tests; two explicitly ignored acceptance tests remained
ignored). The current RustSec database passed against all 765 locked crates.

An all-at-once GNU-linker workspace test attempt saturated the 15-GiB WSL
memory ceiling before tests started. The node suite was rerun successfully with
Clang/LLD, two build jobs, and a Linux-native `/tmp` target. This is build-host
capacity behavior already observed during P10-08, not a runtime-node failure.

## Evidence files

- `RESULT_SUMMARY.json`: declared boundary, exact cgroup ID, rootfs/kernel
  digests, and result.
- `ebpf-status.json`: authenticated node monitor status.
- `cgroup-activation.log`: guest cgroup marker and all userspace-to-BPF CONFIG
  writes.
- `SHA256SUMS`: checksums for the evidence files above.

## Interpretation and remaining production evidence

This closes the ambiguous platform claim: unrelated host cgroups are filtered,
but applications colocated inside one node are one trust domain. It does not
prove or authorize hostile multitenancy. Production still needs the exact
signed candidate on every selected node class, a dedicated operator-managed
cgroup, mandatory eBPF/readiness/alert verification, sustained application TID
attribution, and lifecycle cleanup evidence.

If mutually untrusted applications must share hardware, place them on separate
platform-node processes/VMs today. A future process-per-application mode would
need its own credentials, cgroup, syscall/network boundary, cross-tenant
non-observation tests, escape testing, and capacity validation.

The existing `.prod-validation-p10-08-state.json` environment remains running.

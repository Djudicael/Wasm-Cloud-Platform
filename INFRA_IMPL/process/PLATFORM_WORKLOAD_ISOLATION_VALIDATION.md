# Platform workload-isolation validation

This runbook defines the production contract for applications that share one
`wasm-node` process and the repeatable P10-10 validation. It validates the Wasm
Cloud Platform. Firecracker is only the disposable Linux/VPS-like test fixture.

## Supported production model

The current release supports one mutually trusted application/tenant domain per
platform node:

```toml
[runtime]
isolation_mode = "single-trust-domain"

[ebpf]
enabled = true
required = true
```

Every application still receives WASI capability policy, resource limits, a
dedicated single-thread runtime TID, namespace/application identity, and
node-local service-mesh authorization. These controls contain mistakes and
bound resources. They do not isolate mutually hostile applications because all
components share one native process, UID, address space, capabilities, and node
cgroup.

Production admission rejects an omitted/different isolation mode and optional
or disabled eBPF. A deployment that needs mutually untrusted tenants must use
separate platform nodes/trust domains. Process-per-application support would
need to be implemented and independently validated before that rule changes.

## What eBPF attribution means

- The supervisor registers the dedicated WASI runtime TID with namespace and
  application identity before component execution.
- TCP, file-descriptor, syscall, namespace, and process events are accepted only
  for registered TIDs. Lifecycle cleanup removes TIDs and port bindings.
- The node loader resolves its cgroup-v2 kernel ID. Memory-pressure and block-I/O
  probes reject events originating in other host cgroups.
- TCP-close correlation releases persistent WASI CLI outbound reservations.
- Buffered block writeback can run in kernel-worker context. Such work is not
  promised as exact per-application block-I/O. The platform must never assign it
  to an arbitrary application or claim tenant-grade accounting from it.

Cross-host internal-mesh identity remains out of scope by design. `.internal`
dependencies are node-local and use `every_node` placement.

## Local Firecracker gate

Prerequisites:

- Linux or WSL2 with KVM and Firecracker;
- the pinned Rust and BPF toolchains;
- an existing recorded testbed state;
- a schema-15 node rootfs, built after the candidate binaries and BPF objects;
- the exact state file retained throughout the test.

Build the updated node image:

```bash
cd /mnt/d/dev/Wasm-Cloud-Platform
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target
bash scripts/vm/build-node-rootfs.sh
```

The schema-15 guest mounts cgroup v2, creates
`/sys/fs/cgroup/wasm-node`, emits `WCP_NODE_CGROUP_ID=<id>`, and enters that
cgroup before executing the node. Stale images fail provisioning instead of
silently running without the boundary.

Run the gate against one recorded node:

```bash
bash scripts/vm/validate-workload-isolation-contract.sh \
  --state-file .prod-validation-single-host-state.json \
  --restart-node \
  --evidence-dir INFRA_IMPL/process/prod_validation/evidence/DATE/P10-10-workload-isolation
```

The validator must prove:

1. production admission accepts only `single-trust-domain` plus mandatory eBPF;
2. cgroup-v2 resolution returns a non-zero kernel identity;
3. all seven BPF objects compile from a clean BPF target;
4. the restarted guest reports the dedicated cgroup ID;
5. every loaded BPF `CONFIG` map receives that same ID;
6. the selected node reports seven active programs, mandatory monitoring, no
   monitoring degradation, and ready health;
7. every other recorded platform node remains reachable; and
8. checksummed evidence records exact kernel/rootfs digests and limitations.

After restart, repeat one known application request and readiness check. A node
restart can temporarily remove that node from the load-balancer pool; it must
not make the remaining topology unavailable.

## Production repetition

Repeat the same gate on the exact signed node artifact and every production
host/node class. Additionally retain:

- service-manager/container configuration proving the dedicated cgroup;
- cgroup path and kernel ID at node start;
- `/admin/ebpf/status`, readiness, and relevant Prometheus samples;
- signed artifact, kernel/OS, configuration, and BPF object digests;
- a negative startup result with cgroup v2 unavailable;
- a negative readiness result for partial/terminated eBPF monitoring;
- sustained-concurrency attribution for each deployed application identity;
- lifecycle cleanup results after deploy, stop, removal, and rolling restart;
- operator acknowledgement that all colocated applications share one trust
  domain and that buffered writeback is not per-application accounting.

## Pass/fail decision

Pass only when all checks above are reproducible from the same candidate and no
event is attributed across registered application TIDs. Fail production
admission when the node lacks cgroup v2, the BPF cgroup identity differs, any
required program is inactive, cleanup leaks identities, or the requested threat
model includes mutually untrusted colocated applications.

A local pass closes the platform-source ambiguity and validates the disposable
test fixture. It does not authorize production promotion: signed-candidate,
host-class, monitoring, and operator evidence remain separate release gates.

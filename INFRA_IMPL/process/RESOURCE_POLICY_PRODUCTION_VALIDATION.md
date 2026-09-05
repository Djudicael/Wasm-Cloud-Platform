# Platform resource-policy production validation

This runbook validates resource policy owned by the Wasm Cloud Platform. The
Firecracker topology is a disposable approximation of VPS boundaries; it is
not part of the production platform architecture.

## Resource model

Keep these boundaries distinct:

- the VM, VPS, container, or systemd cgroup hard memory limit;
- `health.max_memory_bytes`, the platform admission and process-health budget;
- an application's `memory_limit`, the maximum linear memory of one instance;
- an application's `max_instances`, its maximum concurrent pool on one node.

The configured platform budget must be lower than the hard host/cgroup limit.
The platform rejects an application when `memory_limit * max_instances` exceeds
that budget. It also retains absolute safety caps of 512 MiB per instance, 100
instances per application per node, and 10 billion fuel units per execution.
Admission is checked on deploy, config update, restored state, and immediately
before spawn. This is a per-application admission bound, not static reservation
of the sum of every application's theoretical peak. Operators must validate the
real concurrent workload mix and retain memory headroom for the kernel,
Wasmtime, caches, queues, telemetry, and the node process itself.

`health.max_memory_bytes` is also bounded at runtime by the smallest detected
physical or cgroup memory limit. That runtime minimum is a final safety check;
it is not a substitute for setting an explicit lower production budget.

## Disk sizing and growth policy

Set `health.min_disk_free_bytes` and `health.min_disk_free_inodes` explicitly.
The filesystem is:

- healthy at or above twice both reserves;
- degraded inside the two-times warning envelope;
- unhealthy/unready below either hard reserve.

Size the state volume from measured steady-state data plus artifact retention,
upgrade/rollback overlap, database growth, logs or queues stored on that volume,
and recovery workspace. Normal operation and a rolling upgrade must remain
above twice the configured reserves. Alert on the exported configured values,
not hard-coded byte or inode constants. Define an owner, expansion procedure,
maximum expansion time, cleanup order, and a tested recovery path before
promotion. Rehearse both byte exhaustion and inode exhaustion.

The production and staging templates expose all three resource values. They are
examples; the operator must replace them with values derived from the selected
node class and volume.

## Local validation

In WSL2, provision at least three 2-GiB platform nodes, the separate application
database when required by the test workload, and observability. Deploy the OIDC
Hub, then run a sustained representative workload before the resource gate:

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target

bash scripts/vm/validate-oidc-capacity.sh \
  --state-file .prod-validation-p10-08-state.json \
  --report-dir /tmp/p10-08-capacity \
  --soak-seconds 120 \
  --only-soak \
  --expected-targets 12

bash scripts/vm/validate-resource-policy.sh \
  --state-file .prod-validation-p10-08-state.json \
  --capacity-summary /tmp/p10-08-capacity/summary.json \
  --oversized-wasm /path/to/a/valid-component.wasm \
  --output /tmp/p10-08-resource-policy.json

bash scripts/vm/validate-alerting.sh \
  --state-file .prod-validation-p10-08-state.json \
  --output /tmp/p10-08-alerting.json
```

The resource validator requires every node to be healthy and accepting traffic,
free bytes and inodes to remain above twice their hard reserves, the effective
memory budget to be below the recorded VM boundary, memory use below the alert
threshold, 600/600 successful soak requests, rejection of a synthetic 2-GiB
application pool on every node, and OIDC database readiness after the negative
test.

## Production promotion evidence

Local success proves the policy can operate inside a bounded Linux guest. A
production candidate still requires:

- the exact signed node artifact and final production configuration;
- the selected VPS/VM/container class and enforced cgroup/systemd limits;
- baseline, expected peak, and failure-mode resource measurements;
- a sustained workload long enough to include cache growth, rotations, GC,
  exports, and rolling replacement;
- a volume-growth and inode-recovery drill within the operational SLO;
- firing and resolved delivery for disk-headroom, hard disk/inode, memory, and
  file-descriptor alerts through the real on-call receiver;
- proof that one node can be removed while the remaining nodes retain the
  defined traffic and resource headroom.

Build-host memory is separate from runtime-node memory. Optimized Wasmtime LTO
linking can require substantially more memory than a 2-GiB runtime guest; size
CI builders independently and keep build failures out of runtime capacity data.


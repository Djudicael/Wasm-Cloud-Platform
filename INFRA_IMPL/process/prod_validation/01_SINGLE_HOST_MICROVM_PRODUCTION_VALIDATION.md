# Single-host microVM production validation plan

## Purpose

Use this plan to validate the platform, an application, and their operational
controls on one Linux or WSL2 computer using the repository's Firecracker
testbed. This is the fastest repeatable production rehearsal and should run
before any multi-host exercise.

This plan validates software behavior under controlled faults. It does **not**
prove survival of a physical host, power, disk-controller, network-interface,
availability-zone, or datacenter failure because every microVM shares one host.

The local testbed scripts are not production provisioning tools. Never give them
production credentials or point them at production networks.

## Validation boundary

This plan is sufficient to validate:

- reproducible platform and WASI application builds;
- three-node platform discovery, scheduling, routing, and convergence;
- the HAProxy front door and application-aware health checks;
- PostgreSQL reachability, schema migration, and application readiness;
- OpenID Connect login, callback, session, and logout behavior;
- Prometheus metric exposure and alert-rule correctness;
- structured log collection, redaction, and correlation;
- OpenTelemetry export, trace continuity, retry, and backpressure behavior;
- functional eBPF loading, scoping, metrics, and userspace fallback;
- node, process, dependency, network, disk, and resource fault handling;
- rolling deployment and rollback behavior; and
- backup creation and restoration into a separate test database.

It is not sufficient to validate:

- independent physical failure domains;
- NATS or PostgreSQL quorum placement across independent hosts;
- provider-managed load balancers, disks, KMS, PKI, or secret managers;
- real availability-zone or regional isolation;
- production maximum capacity; or
- final eBPF overhead on different production hardware or kernels.

Record these items as `NOT VALIDATED`, not `PASS`.

## Required topology

Use the `production-like` preset with at least three **platform nodes**:

```text
Linux/WSL2 host
├── HAProxy front door
├── NATS service microVM (not a platform node)
├── PostgreSQL service microVM (not a platform node)
├── platform-node-1 microVM
├── platform-node-2 microVM
├── platform-node-3 microVM
└── observability services on the host or a dedicated service VM
```

Every platform node contains its own reverse proxy. HAProxy is an additional
front door; it must use application-aware health checks rather than assuming
that every node currently serves every application.

## Evidence and decision rules

Create one validation record per execution. Record:

```text
Run ID:
Date and operator:
Git commit and dirty-worktree status:
Rust toolchain:
Host OS, kernel, CPU, memory, and disk:
Firecracker version:
Guest kernel and rootfs checksums:
Platform release artifact checksums:
Application commit and artifact checksums:
State-file path:
Node count and addresses:
Test-data classification:
Start and end time:
Result: PASS / FAIL / EXCEPTION
Exceptions, owners, approvers, and expiry:
Evidence location:
```

For every test below, retain the command or procedure, timestamps, expected
result, actual result, relevant metrics/logs/traces, and cleanup result.

The run fails when any required check fails. An infrastructure limitation is
`NOT VALIDATED`; it must not be converted into a pass or an undocumented
exception.

## Execution record: 2026-08-23 through 2026-08-27 / single-host run in progress

This section is the live record for the current rehearsal. Do not mark a gate
complete solely because a command was started; retain the command output or its
redacted artifact with the result.

| Gate | Result | Evidence / observation |
|---|---|---|
| Linux/WSL2 environment | PASS | WSL2 x86_64 kernel `6.18.33.2-microsoft-standard-WSL2` |
| KVM access | PASS | `/dev/kvm` is readable and writable |
| Dedicated validation state | PASS | `.prod-validation-single-host-state.json` and its service companion were absent before provisioning |
| Rust formatting | PASS | `cargo fmt --all -- --check` |
| Native workspace check | PASS | `cargo check --workspace --all-targets --exclude http-hello-component --exclude wasi-grpc-echo` |
| Clippy | PASS | `cargo clippy --workspace --all-targets -- -D warnings` |
| Workspace tests | PASS | `cargo test --workspace --quiet`; the initial cold build was slow, then the suite completed |
| Security audit | PASS | `cargo audit --deny warnings`; 1,225 advisories loaded and 749 lockfile dependencies scanned |
| WASI builds | PASS | `hello-axum` and `wasi-grpc-echo` built for `wasm32-wasip2` |
| Existing VM assets | FAIL | Presence alone was an insufficient cache check: `assets/wasm-node-rootfs.ext4` contained the legacy BusyBox/OpenRC `/sbin/init` instead of the current per-node boot contract |
| Provisioning authorization | PASS | User completed the privileged commands in the same interactive WSL terminal that owns the sudo ticket |
| PostgreSQL rootfs image | PASS | `assets/postgres-rootfs.ext4` was created successfully; the subsequent error was limited to cleanup of PostgreSQL-owned files in the temporary build directory |
| Firecracker installation | PASS | Firecracker `v1.15.1` installed and `/dev/kvm` reported accessible |
| Initial topology provisioning | FAIL | `local-test-node-0` failed its 120-second health wait; state was not persisted because the CLI failed before detach |
| Topology provisioning retries | FAIL | Three attempts failed the 120-second node health gate (first node 0, then node 2 twice); no state file was persisted and the failed fixture cleaned up its processes |
| Provisioning root cause | CONFIRMED | The cached 512 MiB node image was built on 2026-05-03 and `/sbin/init` is a symlink to `/bin/busybox`. The current builder requires a custom PID 1 that consumes `wcp.node_id`, `wcp.ip`, and `wcp.gateway`; filename-only cache reuse silently selected the incompatible image |
| Stale-image prevention | PASS | Shell syntax and `git diff --check` pass; provisioning now rejects the legacy image before sudo/VM creation with `expected image schema 2`, and the aggregate image builder rebuilds it instead of skipping by filename |
| Rebuilt platform-node rootfs | PASS | Rebuilt as a 2 GiB ext4 image on 2026-08-23. Schema marker is `2`, `/sbin/init` is a regular executable (`0755`) containing the current boot contract, and `e2fsck -fn` reports a clean filesystem |
| Fourth topology attempt | FAIL | With the rebuilt schema-2 node image, node 2 again missed the 120-second health deadline. No state file was persisted |
| Fourth-attempt diagnosis | IN PROGRESS | All guests accepted `InstanceStart`, but node clones created no `state.redb`, proving userspace did not reach `wasm-node`. The speculative PCI transport change was reverted to the pipeline's original Firecracker virtio-MMIO contract (`pci=off`); serial capture remains enabled for the next attempt |
| MMIO transport remediation checks | PASS | `cargo fmt --all -- --check`, nine `vm-testbed` library tests, and `cargo check -p vm-testbed --bin vm-testbed-cli` passed in WSL with the shared Linux target directory |
| Fifth topology attempt | FAIL | The MMIO retry exited without persisting state. Firecracker metrics after two minutes reported zero rootfs reads, network packets, UART writes, and MMIO exits for every guest |
| Kernel root cause | CONFIRMED | The kernel build enabled `CONFIG_VIRTIO_MMIO` but omitted `CONFIG_VIRTIO_MMIO_CMDLINE_DEVICES`; therefore Firecracker's x86 `virtio_mmio.device=...` arguments could not register the root disk or NIC. The builder now requires the complete boot configuration and emits schema/config sidecars; provisioning rejects pre-schema kernels |
| First schema-2 kernel configuration | FAIL FAST | `olddefconfig` removed `CONFIG_VIRTIO_MMIO` because tinyconfig disabled both the `CONFIG_VIRTIO_MENU` gate and `CONFIG_VIRTIO`. The post-configuration assertion stopped before compilation or replacement of the previous kernel; the builder now enables and verifies the complete parent chain |
| Corrected kernel configuration validation | PASS | Against Linux 6.1.80 Kconfig, `olddefconfig` retains block, virtio menu/core/block/network/MMIO/command-line discovery, ext4, printk, and serial-console settings after enabling the complete dependency chain |
| Schema-2 boot kernel build | PASS WITH eBPF EXCEPTION | Kernel checksum `8fe8203f2ccec608f58501d1f5d7c2540902be233b58966912f42a166ea17622`; schema and all Firecracker boot settings are present. BTF is absent because `dwarves`/`pahole` was not installed and debug-info selection was incomplete. This does not block the next boot test, but eBPF validation remains `NOT VALIDATED` with this artifact |
| BTF builder remediation | RETEST REQUIRED | Future kernel builds install `dwarves`, select the actual DWARF choice and BTF, assert both after `olddefconfig`, and retain `.BTF` while stripping ordinary debug information. A plain `strip` is no longer permitted because it removes BTF |
| Sixth topology attempt | FAIL | The corrected kernel reached the serial console and received Firecracker's `virtio_mmio.device` arguments, but all guests froze during clock initialization before device probing or userspace |
| Guest clock/SMP root cause | CONFIRMED | Serial output ends at `TSC unstable` with no HPET/PM timer available, and reports the second vCPU ignored because tinyconfig set `CONFIG_NR_CPUS=1`. The builder now requires the hypervisor-guest gate, KVM paravirtual clock support, SMP, and at least two CPUs |
| Kernel boot schema | UPDATED | Kernel schema `3` represents virtio command-line discovery plus the KVM clock/SMP contract. Provisioning now rejects the earlier schema-2 kernel before creating VMs |
| Schema-3 BTF kernel build | PASS WITH OBSERVATION | Kernel checksum `92e70aebbfb7c1d94c15cd9ec75e8e10c14df5ee61e2dfbde64fc5bb3e28ff64`; schema 3, KVM clock, SMP/64 CPUs, virtio block/network/MMIO discovery, ext4, serial console, debug info, `.BTF`, and `.BTF_ids` are present |
| BTF `unix_sock` warning | NON-BLOCKING FOR BOOT / RETEST FOR eBPF | `resolve_btfids` reported unresolved `unix_sock` because tinyconfig omitted Unix-domain sockets. The current kernel remains suitable for the topology boot gate, but the builder now requires `CONFIG_UNIX=y`; rebuild before the dedicated eBPF phase and confirm the warning is absent |
| Seventh topology attempt | FAIL | Schema-3 kernel successfully used KVM clock, brought up both vCPUs, registered virtio, found the root disk, and mounted ext4. Every guest then panicked while executing PID 1 with `Requested init /sbin/init failed (error -2)` |
| PID 1 root cause | CONFIRMED | The rootfs contains executable `/sbin/init`, `/bin/sh`, `dash`, and the glibc loader, but the kernel config explicitly disabled both `CONFIG_BINFMT_ELF` and `CONFIG_BINFMT_SCRIPT`. Kernel schema 4 now requires those loaders plus Unix sockets before a guest can be provisioned |
| Schema-4 executable kernel build | PASS | Kernel checksum `2ef9c0a14161f4a116a3321a81392e598dac7bfcd500d4847aa8100becb389c4`; ELF/script loaders, Unix sockets, KVM clock, SMP/64 CPUs, virtio block/network/MMIO, ext4, `.BTF`, and `.BTF_ids` are present. The earlier `unix_sock` BTF warning is absent |
| Kernel baseline hardening | OPEN | The local tinyconfig overlay must be systematically compared with Firecracker's maintained x86_64 6.1 guest config before production promotion. The serial log already shows missing Spectre v2/retpoline and SRSO mitigations; this local test kernel is not approved as a production kernel |
| Eighth topology attempt | FAIL | Schema-4 kernel executed the custom PID 1 and entered `wasm-node`, but Tokio runtime construction failed with OS error 38 (`Function not implemented`), followed by an explicit futex failure and PID 1 panic |
| Async runtime syscall root cause | CONFIRMED | Tinyconfig disabled futex, epoll, signalfd, timerfd, POSIX timers, and high-resolution timers. Kernel schema 5 now requires the async/threading syscall substrate used by Tokio and glibc; related AIO, io_uring, inotify, membarrier, and rseq support is also enabled |
| Legacy NATS image | FAIL / REBUILD REQUIRED | The cached NATS rootfs still has BusyBox `/sbin/init`, cannot execute `/sbin/openrc`, and falls back to a serial login prompt. The current builder's deterministic NATS PID 1 is now marked as NATS image schema 2; aggregate build and provisioning reject the legacy image |
| Schema-5 kernel and NATS rebuild | PASS | Kernel checksum `3dc3a177e027bb9e2c63257fb1227a7637a45ac8ca5819a35c03bfb8bde9dfe5`; all required Tokio/glibc syscall options and BTF sections are present. NATS schema is `2`, PID 1 and `nats-server` are regular executables, and read-only filesystem checks pass for both NATS and platform-node images |
| Production-like topology | PASS | Three 2-vCPU/2-GiB platform nodes are alive at `172.20.0.12-14`; each returns HTTP 200, accepts requests, reports redb healthy, and has a healthy NATS dependency. NATS 2.10.14 is reachable on `4222`, monitoring works on `8222`, and three platform connections are present. HAProxy PID 223984 listens on `127.0.0.1:8088` |
| Empty front door | EXPECTED 502 | HAProxy reaches the per-node Pingora proxies, but `/` has no application route before deployment and therefore returns 502. This is not an infrastructure readiness probe and must not be interpreted as topology failure |
| Node disk policy | DEGRADED / ACCEPTING | Each 2-GiB node image has about 1.6 GiB free. The configured hard minimum is 1 GiB and the warning threshold is twice that value, so detailed health is degraded while readiness remains HTTP 200. Align image capacity and thresholds before production sizing |
| Node memory policy | CONFIGURATION GAP | Detailed health reports process RSS against a configured 4-GiB policy ceiling, while each VM has only 2 GiB. The guest will exhaust memory before this application-level threshold can activate; align the policy below the VM/cgroup limit before production promotion |
| NATS CLI status probe | FIXED / RETEST REQUIRED | Detached status incorrectly sent node HTTP `/healthz` to the NATS VM and printed `unreachable`. It now probes the recorded `nats://` TCP endpoint and reports `connected`, `unreachable`, or `timeout` separately |
| NATS CLI status retest | PASS | Rebuilt CLI reports NATS `alive=true` and `tcp=connected`; all three recorded node PIDs remain alive with HTTP `200 OK`. Formatting, nine `vm-testbed` tests, and workspace Clippy with warnings denied pass |
| PostgreSQL preflight | FAIL FAST / REBUILD REQUIRED | Cached image still uses BusyBox/OpenRC PID 1, and schema-5 lacks SysV IPC/shared memory needed by PostgreSQL. Kernel schema 6 adds and asserts IPC, shared memory, and tmpfs; PostgreSQL image schema 2 installs a deterministic PID 1 for networking, first-boot initialization, role/database creation, signal forwarding, and server supervision |
| Schema-6 kernel and PostgreSQL rebuild | PASS | Kernel checksum `0d3291298300e91348864cfebc9c9fed543d626b3a5fc6a677a2056f02c5a208`; SysV IPC, POSIX queues, shared memory, tmpfs, and ACL support are present. PostgreSQL schema is `2`, PID 1 and PostgreSQL are regular executables, and the ext4 image passes a read-only filesystem check. Existing NATS and three platform nodes remained healthy during the rebuild |
| First PostgreSQL service attempt | FAIL | Guest mounted its rootfs and entered the deterministic PID 1, but `su-exec` failed with `setgroups: Function not implemented`; PostgreSQL never initialized or listened on 5432. Failed VM/TAP cleanup succeeded and no service was persisted |
| PostgreSQL privilege-drop root cause | CONFIRMED | Tinyconfig disabled `CONFIG_MULTIUSER`, removing group/credential syscalls needed to run PostgreSQL as its unprivileged account. Kernel schema 7 now requires multi-user support; running PostgreSQL as root is explicitly rejected as a workaround |
| Second PostgreSQL service attempt | FAIL | With kernel schema 7, the guest completed privilege drop and reached database initialization, but Alpine's generic `/usr/bin/initdb` entry point failed its companion-binary lookup with `program \"postgres\" is needed by initdb but was not found in the same directory`. PID 1 then exited and Firecracker cleaned the failed VM/TAP without recording a service |
| PostgreSQL binary-selection root cause | CONFIRMED / REBUILD REQUIRED | Alpine 3.21 exposes generic `/usr/bin` symlinks through `/usr/libexec/postgresql`, while the installed PostgreSQL 17 server and client executables live together in `/usr/libexec/postgresql17`. PostgreSQL image schema 3 now verifies that directory at build time and invokes `initdb`, `postgres`, `pg_isready`, `psql`, and `createdb` from it explicitly. This prevents version-selector ambiguity and guarantees `initdb` finds its matching server binary |
| PostgreSQL schema-3 image rebuild | PASS | Alpine PostgreSQL `17.11` was installed; the builder verified all five required executables in `/usr/libexec/postgresql17`, wrote image schema `3`, and produced a clean 1-GiB ext4 filesystem. The existing platform, NATS, and HAProxy processes were not replaced |
| Third PostgreSQL service attempt | PASS | Service `oidc-postgres` is alive as PID `289481` at `172.20.0.20:5432`. The guest initialized with SysV shared memory, started PostgreSQL 17.11 on IPv4/IPv6 and its Unix socket, reported ready, created the `oidc` role, and completed database creation without error. Host TCP connection succeeded and detached topology status records the service |
| PostgreSQL network-policy scope | PARTIAL | `pg_hba.conf` trusts only local Unix-socket connections and requires SCRAM-SHA-256 for `172.20.0.0/24`; host TCP readiness is proven. Credential authentication and connectivity from a workload on each platform node remain application-deployment gates |
| OIDC Hub build and first deployment | PASS | Locked frontend install audited 101 packages with zero reported vulnerabilities; the frontend and backend built for `wasm32-wasip2`. Migrations through V37 completed against PostgreSQL 17.11 using `wasi-pg-client 0.2.0`, seed data completed, and both WASI artifacts were deployed with peer manifests across the three-node platform |
| Application-to-PostgreSQL path | PASS FOR SCHEDULED WORKLOAD | The deployed backend authenticated with SCRAM from the platform network and `GET /health/ready` through HAProxy returned `{"checks":{"database":"ok"},"status":"ready"}`. This proves the application path that previously returned database failure; forced execution/failover on every individual node remains a later resilience gate |
| Same-origin OIDC gateway | PASS | HAProxy configuration validation passed. Frontend `/`, SPA `/realms/master`, login page, discovery, backend readiness, and seeded login API all returned 200; discovery issuer equals `http://127.0.0.1:8088`. Frontend and backend health use separate application-aware pools |
| Migration/redeployment repeat | PASS | A second complete deployment exited successfully. `_migrations` already existed and was skipped, migrations and seed completed, the existing admin API key was detected, and all readiness/frontend/SPA/login routes remained HTTP 200 afterward |
| Local seed-secret logging | FAIL FOR PRODUCTION / ACCEPTED ONLY IN DISPOSABLE TEST | The development seeder writes the test administrator password, confidential-client secret, and one-time API key to its output. The repeat-run log was created with a restrictive umask, but production migration/seed tooling must never print credentials or tokens and must source them from an external secret manager |
| WSL secret-file permissions | REMEDIATED | A state-adjacent directory under `/mnt/d` remained mode `0777` despite requested `0700`/`0600`, because this WSL Windows mount does not enforce those Unix mode changes. Deployment and teardown now derive one exact state-keyed directory under `$XDG_RUNTIME_DIR` (or `/tmp`) on the Linux filesystem. The live keys and credentials were moved there; verified modes are `0700` for both directories and `0600` for credentials, and the unprotected checkout copy no longer exists |
| Verification response confidentiality | REMEDIATED | The first deployment left fixed, world-readable `/tmp/oidc-*` response files, including the seeded login token response. Those exact files were removed. Verification now uses one mode-`0700` `mktemp` directory and an exit trap removes the login payload and every HTTP response on both success and failure; a complete redeployment passed after the change |
| Manual browser journey | PASS (USER-VALIDATED) | The operator validated the interactive frontend and application journey through the HAProxy public origin after automated readiness, discovery, SPA, and seeded-login checks passed. Negative/expiry cases remain separate automated security gates rather than being inferred from this attestation |
| Host capacity preflight | PASS FOR CURRENT REHEARSAL | WSL exposes 24 CPUs, 15 GiB RAM plus 4 GiB swap, 683 GiB free on the Linux filesystem, and 471 GiB free on `/mnt/d`. This is sufficient for the current six microVM/process services and disposable telemetry stack; it is not production sizing evidence |
| Release provenance snapshot | PARTIAL | Source commit is `4abbc7db5087862de0331f03a2b7904433dba727` with a dirty worktree intentionally preserved. Rust is pinned to 1.97.1. SHA-256: platform lock `8f7c549d...`, kernel `352fd0be...`, node image `932c118f...`, NATS image `76dace70...`, PostgreSQL image `811cc23f...`, frontend WASI `d2d032b3...`, backend WASI `0e9fd62b...`. A standards-based SBOM and clean promotion build are still missing |
| Effective node configuration review | GAPS RECORDED | Unique node IDs and routable bridge addresses are correct. Local admin auth is enabled over HTTP with only a shared write token; `/metrics` requires that token. Proxy/admin TLS is disabled, key source is ephemeral `generate`, OTLP output is unset, configured memory ceiling is 4 GiB inside 2-GiB VMs, eBPF is configured enabled but the release lacks active probes, and local test credentials must not be promoted |
| Metrics endpoint contract | FIXED / TESTED | Runtime serves authenticated `/metrics`; `/status/metrics` is absent. Stale middleware documentation/tests that treated `/status/metrics` as public were removed, Prometheus rule documentation now names `/metrics` with a read token, and all 45 proxy authentication tests pass. The live test still uses the write token because its image predates a dedicated monitoring token |
| Disposable observability stack | PASS | State-scoped Podman services run Prometheus 3.5.0, Alertmanager 0.28.1, node-exporter 1.9.1, NATS exporter 0.17.3, PostgreSQL exporter 0.17.1, and OpenTelemetry Collector Contrib 0.130.1. Exact container IDs and the Linux-runtime configuration directory are recorded in companion state for teardown |
| Prometheus scrape coverage | PASS | Ten live targets report `up=1`: three separately labeled platform nodes, HAProxy, NATS, PostgreSQL (`pg_up=1`), WSL host, OpenTelemetry Collector, Prometheus, and Alertmanager. HAProxy metrics bind only to `127.0.0.1:8405`; all telemetry UI/exporter binds are loopback-only |
| Prometheus configuration/rules | PASS WITH ONE SEMANTIC GAP | `promtool check config` passes with three rule files and 19 rules; live rule evaluation health is `ok`. The existing `AdminAuthDisabled` expression depends on `process_start_time_seconds`, which is not emitted by platform-node metrics and therefore cannot prove auth mode; add an explicit auth-enabled gauge before production acceptance |
| Alert delivery and recovery | PASS (TELEMETRY FAILURE) | Stopping only the recorded Collector container caused `TelemetryCollectorDown` to progress through pending to firing and arrive in Alertmanager with stable cluster/environment/instance labels. Restarting the same container restored `up=1`; Alertmanager returned no active alerts and the OIDC database readiness remained ready |
| OpenTelemetry pipeline | RECEIVER ONLY / NOT APPLICATION-INTEGRATED | The Collector accepts OTLP on loopback and exposes self-metrics, but effective node configuration has `otlp_endpoint=null`. No platform/app logs or traces reached it, so trace continuity, buffering, retry, sampling, and shutdown flushing remain unvalidated |
| PostgreSQL interruption/recovery | PASS | The exact recorded PostgreSQL Firecracker PID `289481` was paused with `SIGSTOP`. Within the bounded health window, HAProxy returned 503 for backend readiness, the independent frontend remained HTTP 200, the PostgreSQL exporter target went down, and `PostgreSQLExporterDown` fired in Alertmanager. The paused VMM stayed near 1% memory without CPU/log escalation. `SIGCONT` restored the same VM; readiness returned `database=ok`, `pg_up=1`, and the alert resolved without redeployment |
| NATS interruption/recovery | PASS AFTER REMEDIATION | Pausing only recorded NATS Firecracker PID `223906` made the exporter target go down and fired `NatsExporterDown`; frontend and backend data-plane routes remained HTTP 200. The original nodes incorrectly reported NATS as healthy for more than 65 seconds because `NatsHealthWatcher` refreshed its timestamp without probing the server. A first canary using `Client::flush()` also failed because flush only confirms writes to the local TCP buffer. The final implementation performs a two-second request/reply to `$JS.API.INFO` every five seconds, proving both NATS and required JetStream responsiveness. After rolling the rebuilt image to all three nodes, a whole-cluster freeze made every node health endpoint return HTTP 503 within 12 seconds while both application routes stayed available. `SIGCONT` restored all node health endpoints to HTTP 200 and resolved the alert within 12 seconds without redeployment |
| Platform-node rolling replacement | PASS | The corrected base image (`sha256:40e1e1c58430fc82e1ec775ed392e2b063e28a93aada85c0aa00e60902ebe7dc`) passed read-only `e2fsck`. Nodes 2, 1, and 0 were restarted sequentially from exact recorded state. Each node became healthy, reconstructed both OIDC deployments from desired state, and served traffic; the HAProxy backend route remained HTTP 200 throughout the observed post-restart gates and no alert remained active |
| Single platform-node pause/recovery | PASS | Recorded node 0 PID `325604` was paused with `SIGSTOP`. Its admin endpoint timed out, `PlatformNodeDown` fired with `node=local-test-node-0`, and HAProxy metrics marked node 0 `DOWN` with `L4TOUT` in both OIDC frontend and backend pools while nodes 1 and 2 remained `UP`. Public frontend and backend readiness remained HTTP 200. `SIGCONT` restored node 0 health to HTTP 200 and Alertmanager cleared without deployment changes |
| Abrupt platform-node termination/rebuild | PASS | After command-line verification, recorded node 1 Firecracker PID `325535` received `SIGKILL` and exited. `PlatformNodeDown` fired for only node 1 while frontend and backend stayed HTTP 200. State-driven `restart-node` replaced it as PID `327249`; within 20 seconds it was accepting requests with both OIDC deployments reconstructed, the alert cleared, and public backend readiness remained HTTP 200 |
| eBPF build and packaging gate | REMEDIATED IN SOURCE / RUNTIME RETEST REQUIRED | Root cause was a chain of independent gaps: Aya 0.14 API drift, no pinned nightly/BPF-linker workflow, seven separately compiled objects treated as one object, only one event ring consumed, loaded program owners dropped immediately after initialization, and the guest image neither enabling the feature nor packaging the objects. The source now passes `cargo clippy -p node --features ebpf --all-targets -- -D warnings`; all seven `bpfel-unknown-none` ELFs build with warnings denied using pinned `nightly-2026-08-20` and `bpf-linker` 0.11.0; the loader retains every enabled object, consumes every ring (including namespace enforcement), and the rootfs builder enables the feature and installs `/opt/wasm-node/ebpf/<object>.o`. Reproducible installer/build scripts are under `scripts/ebpf/`. This closes the build/package P0, but kernel verifier acceptance, least-privilege capability loading, real event generation, fallback, and overhead remain `NOT VALIDATED` until a rebuilt microVM canary completes Phase 6 |
| PostgreSQL logical backup/restore | PASS WITH APPLICATION-REPOINT GAP | `scripts/vm/validate-postgres-backup.sh` read the recorded service address, created a private custom-format dump, restored it with `--exit-on-error` into an isolated PostgreSQL 17 container, and compared source/restored metadata. Both contained 25 public tables and 11 migration rows; backup SHA-256 was `7cbcd23d036553d7dde28d3140bfa8e2ee37be355d9c01307672f6b31a0afb35`. The exact labeled container and private runtime directory were removed. This proves logical restorability, not application readiness against the restored endpoint, backup encryption/retention, or production RPO/RTO |
| Post-remediation repository checks | PASS / eBPF RUNTIME PENDING | In WSL with `CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target`: `cargo fmt --all -- --check`, shell syntax for changed scripts, `cargo clippy -p node --features ebpf --all-targets -- -D warnings`, all seven BPF object builds with `-D warnings`, and 117 `ebpf-monitor` feature-enabled tests pass. Earlier required workspace checks and complete messaging/proxy suites also passed. Global `git diff --check` remains unsuitable on this checkout because unrelated pre-existing CRLF worktree conversions are reported as trailing whitespace. Runtime verifier/event/overhead validation is still pending Phase 6 |
| Final live-state snapshot | PASS / HISTORICAL PRE-TEARDOWN | Before authorized teardown, NATS PID `223906`, PostgreSQL PID `289481`, and platform node PIDs `325604`, `327249`, and `325384` were alive; every node health probe, both public routes, all 10 Prometheus targets, and Alertmanager were healthy. The later authorized-teardown row records removal of this environment |
| Manual Prometheus UI validation | PASS (USER-VALIDATED) | The operator completed manual testing through `http://localhost:9095` after automated validation confirmed 10 targets up, zero targets down, and no active Alertmanager alerts |
| Authorized teardown | PASS | After explicit operator authorization, `scripts/vm/destroy-testbed.sh` validated the selected state and removed its six exact state-labeled observability containers, recorded HAProxy PID/config/log, three platform-node VMs, NATS VM, PostgreSQL VM, exact TAP devices and bridge, state-derived OIDC credentials, and lifecycle state files. A post-teardown audit confirmed every recorded PID/device/container absent and ports 8088, 9095, and 9093 closed. No broad process or network matching was used |
| 2026-08-25 production-like reprovision | PASS / HISTORICAL PRE-TEARDOWN | The exact state `.prod-validation-single-host-state.json` recorded NATS PID `170213`, PostgreSQL PID `170523`, and three 2-GiB platform nodes at `172.20.0.12-14`. After Part 4 cleanup and the final rolling restart, node PIDs were `387278`, `368191`, and `368273`. Every node health probe returned HTTP 200, HAProxy was available at `http://127.0.0.1:8088`, OIDC readiness returned `database=ok`, and no alert remained active. The later 2026-08-26 teardown row records removal of this environment. |
| eBPF guest kernel and verifier gate | PASS | Linux `6.1.80` was rebuilt with BTF, tracing, syscall tracepoints, kprobes, modules, and the complete Firecracker boot contract. All seven objects (`process_tracker`, `tcp_monitor`, `fd_watcher`, `mem_pressure`, `disk_monitor`, `syscall_counter`, and `namespace_enforcer`) loaded and attached on all three guests with no verifier error. The FD watcher used the supported `filp_close` fallback where `do_filp_close` was unavailable. |
| eBPF verifier compatibility defects | FIXED / PASS | Linux 6.1 rejected implicit struct padding passed to ring-buffer helpers and a variable-size namespace header read. Event wire padding is now explicit and zeroed on both sides, the forged-header scan uses verifier-provable fixed reads, and all seven BPF ELFs rebuild with pinned `nightly-2026-08-20` and `bpf-linker` 0.11.0. The reusable verifier-log summary is `scripts/ebpf/summarize-verifier-log.jq`. |
| eBPF identity boundary | PASS FOR IN-PROCESS WASI HTTP / STRONGER ISOLATION OPTIONAL | Initial runtime evidence showed the node's own `bpf(2)`, socket, and worker-thread activity classified as Wasm-instance activity, producing false privilege-escalation incidents and kill requests. Root causes were PID/TGID filtering, late namespace-map wiring through a failed `Arc::get_mut`, registering maps from the thread being monitored, discarding the event TID in userspace, and falling back to killing the largest instance. Each WASI HTTP instance now owns a dedicated single-thread Tokio runtime. The supervisor learns that Linux TID, registers its namespace/application identity from an unmonitored control thread before releasing application execution, mirrors it into all three maps, preserves the TID through parsing and dispatch, and can kill the exact matching instance. A dedicated process or cgroup remains the stronger production boundary when tenant-grade isolation is required. |
| WASI HTTP attribution validation (Phase 6, Part 1) | PASS | The corrected rootfs (`sha256:30fe108dabda354d68e755a03cc0a299c8372f91d6a667d6bd239c5fa6975275`) was rolled one node at a time. Live kernel-map registration recorded distinct frontend/backend TIDs on every node: node 0 frontend `70`, backend `71`; node 1 frontend `68`, backend `69`; node 2 backend `68`, frontend `69`; every registration reported `map_count=3`, namespace `oidc`, and the exact deployment ID. The parser/dispatcher regression test proves that a syscall event retains its TID and resolves it to the registered namespace/application, while the supervisor regression test proves exact-TID termination. Twenty thousand direct backend readiness requests produced no false security event, no parse error, and no instance churn; all three nodes continued serving both deployments and PostgreSQL readiness remained `database=ok`. The high default per-CPU threshold did not emit a rate anomaly during this attribution-only run; deterministic event generation and counter assertions are explicitly Part 2 rather than evidence for ring-pressure testing. |
| eBPF Prometheus mode gauge | DEFECT FIXED / PASS | Runtime/admin status reported eBPF active while Prometheus exported `wasm_ebpf_active 0`. The registration macro evaluated each constructor twice, registering one collector and returning a disconnected handle. It now evaluates once, and a regression test gathers the registry value. Following rolling replacement of all nodes, Prometheus reports `wasm_ebpf_active=1` for `local-test-node-0`, `-1`, and `-2`. |
| eBPF deterministic probes (Phase 6, Part 2) | PARTIAL PASS | Exact-identity process start/exit, known-syscall, FD open/close, TCP connect/accept/send/receive/close, PostgreSQL allow, and NATS policy-deny assertions passed with per-type Prometheus evidence and zero parse errors. A false retransmit heuristic was removed and the corrected node reports zero retransmits. Direct reclaim emitted `MEDIUM` and activated backpressure, but the deliberately undersized guest reached OOM before its counter could be scraped; application `sync_all` writes did not move the block counter. Memory-counter and application block-I/O assertions remain open, and PID-1 OOM behavior plus the 4-GiB configured ceiling inside a 2-GiB VM are recorded production blockers. |
| eBPF block-I/O accuracy (Phase 6, Part 3) | PASS WITH ATTRIBUTION LIMITATION | Linux 6.1 issue/complete records now use the real raw-tracepoint offsets, correlate by device/sector/operation, retain issue identity and cgroup, decode native `dev_t`, classify read/write/flush, and export completed bytes. A 256-MiB synchronous canary write produced 750 slow events and 238,960,640 slow-write bytes at the validation-only 1-ms threshold with zero parser errors; records reported `254:0`, nonzero sectors, `bytes == nr_sector * 512`, and measured latency. Cold artifact reads retained the exact application identity. Buffered ext4 writes correctly reported the kernel writeback thread as unregistered instead of falsely claiming the WASI deployment; per-application buffered-write attribution requires a process/cgroup boundary and cgroup-aware writeback accounting. The 50-ms default was restored before rolling schema 4 through all nodes. |
| eBPF ring-buffer pressure (Phase 6, Part 4) | PASS | Every BPF object now records failed ring reservations/outputs in a per-CPU counter. Userspace exports per-monitor loss, drop-counter read errors, bounded dispatcher depth/capacity, and saturation transitions. A 100,000-iteration FD burst deliberately caused 87,292 kernel drops and 19 queue-saturation transitions with zero parse errors; a second burst caused both Prometheus and Alertmanager loss/saturation alerts to fire. High-rate success logs initially caused one scrape outage, so ordinary FD/TCP records moved from info to debug. The final 100,000-iteration retest completed HTTP 200 with zero failures across 10 node/OIDC health samples, zero queue saturation, zero parse errors, and bounded memory. The canary and route were removed before the final rolling node update. |
| 2026-08-26 Part 4 authorized teardown | PASS | After explicit operator authorization, the canonical state-scoped teardown removed the six recorded observability containers, HAProxy PID `294922` plus its generated config/log, NATS PID `170213`, PostgreSQL PID `170523`, platform-node PIDs `387278`, `368191`, and `368273`, their five TAP devices, bridge `br-local-test`, state-derived OIDC secrets, observability runtime directory, and both lifecycle state files. Post-teardown checks found every exact PID, container, device, and generated file absent. No listeners remained on `8088`, `8405`, `9095`, `9093`, `4317`, or `4318`. Rootfs/kernel assets and repository work were intentionally retained so Part 5 can begin from a freshly provisioned environment. |
| PostgreSQL unattended provisioning | DEFECT FIXED / PASS | The service wrapper initially blocked on an expired terminal-bound `sudo` ticket and created no VM. Under WSL it now invokes the exact CLI through `wsl.exe -u root`, matching platform provisioning. Service `oidc-postgres` then became reachable at `172.20.0.20:5432`. |
| OIDC application deployment (2026-08-25) | PASS | The locked frontend audited 101 npm packages with zero reported vulnerabilities; both WASI components built; migrations through V37 and repeatable seed data completed against PostgreSQL 17.11 using `wasi-pg-client 0.2.0`; frontend, SPA route, discovery issuer, seeded login, and backend readiness all passed through the same-origin HAProxy gateway. Readiness returned `{"checks":{"database":"ok"},"status":"ready"}`. |
| Focused Playwright production journey | PASS | Playwright 1.62.1 Chromium was installed in WSL. Six focused login/dashboard tests passed against `http://localhost:8088`, both before and after the rolling node update: form rendering, invalid credentials, successful login/redirect, empty-field validation, authenticated navigation, and API-backed dashboard statistics. The final serial run completed 6/6 in 4.9 seconds. |
| Full application Playwright suite | FAIL / APPLICATION TEST DEBT | The 44-test Chromium run at its default 12 workers completed with 16 passes, 3 flaky passes, and 25 failures. Evidence shows both concurrency timeouts and deterministic test defects: stale seeded identity expectations (`admin@localhost` versus `admin@example.com`), ambiguous text locators resolving multiple elements, and cross-test prerequisites such as expecting an API key whose creation failed. The platform and database remained ready after the run. Do not use this suite as a production gate until tests use isolated data, stable role/test-id selectors, bounded worker count, and no order-dependent fixtures. |
| Disposable observability targets | PASS | Prometheus, Alertmanager, HAProxy, NATS, PostgreSQL, host, OpenTelemetry Collector, and all three platform-node targets are up: `sum(up)=10` and `count(up == 1)=10`. HAProxy request-rate data was observed during the browser run. |
| Logs and distributed traces | FAIL / NOT IMPLEMENTED END TO END | Nodes emit structured JSON serial logs, but no bounded log agent forwards them. The OTLP Collector is healthy on 4317/4318, yet `otelcol_receiver_accepted_spans` and `otelcol_receiver_accepted_log_records` have no series after the Playwright journey. Source inspection confirms `logging.otlp_endpoint` is loaded but not wired into the node subscriber; `metrics::tracing_setup::init_tracing` is unused and currently panics on exporter initialization. No correlated HAProxy-to-WASI-to-PostgreSQL trace exists. Phase 5 remains open. |
| Rolling node image update | PASS | The eBPF metric fix was promoted by restarting `local-test-node-0`, `-1`, and `-2` one at a time from the corrected rootfs. Public OIDC readiness remained `database: ok` after every replacement, all nodes reconverged healthy, PostgreSQL/NATS/observability were preserved, and the focused Playwright gate passed afterward. |
| 2026-08-26 Part 3 source gates | PASS WITH DEPENDENCY NOTICE | In WSL, `cargo fmt --all -- --check`, the required native workspace all-target check, workspace all-target Clippy with warnings denied, eBPF-feature node Clippy with warnings denied, all 118 eBPF monitor tests, 87 common tests, 64 runtime unit plus 7 integration tests, 38 supervisor unit plus 10 integration tests, 98 node library/binary unit plus 10 integration tests, all seven BPF object builds, changed-script shell syntax, and explicit `wasm32-wasip2` release builds for `hello-axum`, `http-hello-component`, and `wasi-grpc-echo` pass. Rust still emits the separately tracked future-incompatibility notice for `proc-macro-error2 2.0.1`. |
| 2026-08-26 Part 4 source gates | PASS WITH DEPENDENCY NOTICE | Formatting, required native workspace all-target check, workspace all-target Clippy with warnings denied, eBPF-feature node Clippy with warnings denied, all 119 eBPF monitor tests, clean builds of all seven BPF ELFs, shell syntax, scoped whitespace checks, and explicit `wasm32-wasip2` release builds for `hello-axum`, `http-hello-component`, and `wasi-grpc-echo` pass in WSL. The isolated BPF clean was also verified with the shared native target variable set. The existing `proc-macro-error2 2.0.1` future-incompatibility notice remains separately tracked. |
| 2026-08-27 Phase 6 Part 5 failure/degraded modes | PASS / OBSERVABILITY GAP SUBSEQUENTLY CLOSED | A state-driven runner validated real eBPF capability removal, a physically BTF-stripped guest kernel, deterministic permission/program/probe/consumer failures, optional fallback, mandatory startup refusal, Prometheus state, alert separation, peer availability, canary continuity, and clean recovery. All three nodes finished healthy and node 0 reported `ebpf_active=true`, `monitoring_degraded=false`. The HAProxy exporter gap observed during this part was fixed and independently retested before Part 6. |
| 2026-08-27 Part 5 source and audit gates | PASS WITH DEPENDENCY NOTICE | Required native workspace check, workspace Clippy with warnings denied, eBPF-feature node Clippy, 120 eBPF-monitor tests, vm-testbed unit/doc tests, all seven BPF ELFs, three explicit `wasm32-wasip2` release builds, formatting, and changed-script syntax pass in WSL. `cargo audit --deny warnings` initially found yanked `chacha20 0.10.1`; `Cargo.lock` now resolves non-yanked `0.10.2`, and the audit passes. Rust still reports the separately tracked `proc-macro-error2 2.0.1` future-incompatibility notice. |
| HAProxy Prometheus gap remediation | FIXED / PASS | Production-like HAProxy generation now exposes the native exporter only on `127.0.0.1:8405`; a state-driven reload helper validates and replaces only the recorded HAProxy process and atomically updates its lifecycle state. Live HAProxy PID `99344` serves metrics, Prometheus reports the `haproxy` target `up`, and the final alert set is empty. |
| eBPF lifecycle cleanup (Phase 6, Part 6) | PASS | `scripts/vm/validate-ebpf-lifecycle-cleanup.sh` completed three WASI HTTP deploy/forced-stop/cold-start/remove cycles across all three nodes and one full rolling node restart. Every restart/deploy produced a fresh TID registration; all five kernel identity maps matched the active identity count. Final state on every node is zero active TIDs, tombstones, port bindings, map entries, and bpffs pins; seven node-scoped programs remain attached as designed. Removed routes no longer return 2xx, per-app Prometheus series disappear, no stale identity receives new events, HAProxy remains scrapeable, and Prometheus has no active alert. |
| Part 6 lifecycle defects | FIXED / PASS | Undeploy now removes routes before instances, removes the empty supervisor pool, retains artifacts only for the GC grace period, and deploy clears the undeploy marker. Local restore and peer bootstrap exclude grace-period apps and routes. Health metrics delete absent application label series. `NatsBus::publish` flushes before returning so short-lived CLI commands cannot report success while dropping buffered control events. The canonical app deployer now supports both Wasm binaries and `cdylib` components. |
| 2026-08-27 Part 6 source and audit gates | PASS WITH DEPENDENCY NOTICE | WSL gates pass: formatting; required native workspace all-target check; workspace all-target Clippy with warnings denied; eBPF-feature node Clippy; storage 51 unit plus 12 integration tests; messaging 13; metrics 11; supervisor 38 unit plus 10 integration tests; eBPF monitor 121; explicit release `wasm32-wasip2` builds for `http-hello-component` and `wasi-grpc-echo`; changed-script syntax; and `cargo audit --deny warnings` over 749 dependencies. The separately tracked `proc-macro-error2 2.0.1` future-incompatibility notice remains. |
| 2026-08-29 eBPF overhead (Phase 6, Part 7) | PASS | The final rootfs was measured with two alternating fallback/active blocks, two 20,000-request rounds per mode per block, and concurrency 32: 80,000 requests per mode and 160,000 total. Active eBPF caused 0.12% median-throughput loss, +1.11 ms median p99, +1.72% node CPU per 1,000 requests, +12.00% node context switches, and +8.62 MiB peak RSS; application-TID CPU increased 2.95% and its context switches increased 0.32%. All are inside the predefined 5% throughput/meaningful-p99, 15% CPU, 25% context-switch, and 64-MiB RSS limits. There were zero request failures, parse errors, ring drops, or queue saturations while 170,842 eBPF events were observed. Slow-I/O degraded-state recovery, bounded recovery-worker lifecycle, and kernel CONFIG-map hot reload defects found during the audit were fixed and validated in the guest before this final rerun. |
| 2026-08-29 OIDC post-eBPF validation (Phase 6, Part 8) | PASS WITH RECORDED PRODUCTION LIMITS | PostgreSQL 17.11 was reprovisioned, OIDC migrations through V37 and repeatable seed data completed, and the versioned frontend/backend WASI components passed database readiness through the same-origin HAProxy gateway. The focused Chromium login/dashboard journey passed 6/6 before and after a one-at-a-time restart of all three platform nodes (5.2 s and 4.9 s). Every replacement reconstructed both applications and registered fresh exact-deployment TIDs in all five identity maps. Runtime evidence tied PostgreSQL connects to the backend identity; process/syscall, FD, TCP, and block-I/O counters were present. Final Prometheus state was 10/10 targets up, three active eBPF nodes, 6,081 processed events, and zero monitoring degradation, parse errors, or queue saturations. The 2-GiB guest images remained traffic-serving but reported low-disk health, and buffered writeback/per-application RSS still require a stronger process/cgroup boundary for tenant-grade attribution. |
| 2026-08-29 controlled platform-node drain (Phase 7, Part 1) | PASS AFTER OPERATOR-DRAIN REMEDIATION | Firecracker's guest reboot action alone did not invoke the platform application-drain path. A scoped `wasm-ctl node --target NODE drain` command was added, ambiguous untargeted drains are refused, and the target now fences new traffic/readiness before instance shutdown. Node 0 became HTTP 503-ready immediately, HAProxy removed both pools, both instances reached zero, and an orderly guest stop followed. With node 0 absent, 20 paced OIDC readiness requests at 4 requests/s passed 20/20 in 33–52 ms. A deliberately unpaced 30-request burst produced 29 HTTP 200 and one node-local HTTP 429, proving admission limits—not availability—must be aligned with the Phase 8 load profile. State-driven rebuild restored fresh identities; all three nodes were rolled to rootfs `b6512b8b...`, Playwright passed 6/6, Prometheus returned to 10/10, and no alert remained. |

| 2026-08-29 controlled network faults (Phase 7, Part 2) | PASS AFTER APPLICATION-TIMEOUT AND STALE-DEPLOYMENT REMEDIATION | Exact node-0 netem, NATS, and PostgreSQL faults were injected and individually removed. Latency/loss/bandwidth controls behaved as configured; NATS loss made only node 0 not-ready and recovered without a reconnect storm. The first PostgreSQL partition exposed an unbounded WASI database readiness connection behind HAProxy's 60-second server timeout. The OIDC backend now enforces a two-second database health deadline: the live retest returned bounded HTTP 503 in 2.013 seconds, subsequent circuit-open responses took 4-9 ms, HAProxy withdrew only node 0, and peer traffic stayed HTTP 200. Automatic recovery occurred after the circuit-breaker cooldown without redeployment. A superseded content-addressed backend initially left stale alert labels; the OIDC deployer now removes old same-component versions after the replacement passes health, waits for their metric series to disappear, and the node app API excludes undeployed grace-period artifacts. Rootfs `e23eeac...` was rolled through all three nodes; only the two current OIDC deployments are listed, all pools are up, Playwright passes 6/6, Prometheus reports 10/10 targets, no alert or fault rule remains, and every TAP uses `fq_codel`. |
| 2026-08-29 PostgreSQL failure modes (Phase 7, Part 3) | PASS AFTER DEADLINE, EXPORTER, AND MIGRATION REMEDIATION | A reusable state-scoped runner proved prompt invalid-credential failure, 197 observed sessions against PostgreSQL's 200/3 maximum/reserved configuration, backend HTTP 503 with an independent HTTP 200 frontend during exhaustion, automatic pool recovery, a two-second statement deadline, a sanitized application lock-timeout response in 2.029 seconds, and serialized migrations. An advisory-lock competitor failed in 10.06 seconds with SQLSTATE `55P03`; after release the migrator passed in 0.08 seconds with 11/11 unique migration records. Runtime database deadlines are now deployment identity, exporter queries have bounded connect/statement/lock/transaction deadlines, and all three native migration entry points apply each migration plus tracking row atomically under the same session advisory lock. The final focused Playwright gate passed 6/6, OIDC readiness was `database=ok`, all Prometheus alerts were clear, and the environment remains live for Part 4. |
| 2026-08-29 OIDC capacity and short soak (Phase 8, Part 1) | PASS FOR THE CONFIGURED LOCAL ENVELOPE | A paced 50/25/20/5 frontend/discovery/readiness/login mix established HAProxy's configured 100-requests-per-10-seconds per-client stick-table as the first saturation boundary. At 5 requests/s, 600/600 requests passed a 120-second short soak; route p99 values were 5.2/54.7/57.1/166.4 ms. At 15 requests/s the ramp admitted 74/300 and returned 226 deliberate HTTP 429 responses; 30 and 60 requests/s were completely admission-limited before host resources saturated. With node 0 drained and absent, the representative 5 requests/s mix passed 151/151, while an earlier 15 requests/s readiness run admitted 98/450 and rejected 352. The conservative envelope for this exact front-door policy is therefore 5 requests/s per client IP, including N−1 headroom; this is not the host's raw compute maximum. Node memory peaked at 27%, process RSS at 578 MiB, CPU at 0.09 core, PostgreSQL at two sessions, HAProxy queue at zero, and NATS/eBPF error-pressure signals at zero. All three nodes were restored, OIDC database readiness passed, Prometheus returned to 10/10, and no alert remained. |
| 2026-08-29 OIDC upgrade and rollback (Phase 8, Part 2) | PASS WITH RELEASE-ARTIFACT LIMITATION | The last-known-good backend `vdc1902621308` and candidate identity `vdc1902621308-phase8-candidate` ran concurrently against a transactional additive schema probe. Candidate readiness passed on all nodes, the public route cut over and rolled back while 450/450 paced requests stayed HTTP 200 (p99 68.5 ms), and migration tracking returned from 11/11 to 11/11 after exact probe cleanup. An admitted synthetic candidate with a malformed database URL returned HTTP 500, stopped at canary, and was undeployed without deleting the current application or database state. The final route inventory contains only the original frontend/backend, no Phase 8 metric series remains, Playwright passes 6/6, Prometheus is 10/10 with no alert, and eBPF is active on three nodes. Both release identities intentionally used the same tested Wasm SHA-256 because no clean newer artifact existed; repeat this gate with the real distinct release digest before production promotion. A host-suspend clock-skew failure was also fixed by image-schema-6 boot-time and continuous Chrony synchronization. |
| 2026-08-29 OIDC disaster recovery (Phase 9) | PASS WITH DATABASE-TIME, KMS, AND RETENTION LIMITATIONS | A state-scoped recovery runner created a PostgreSQL 17 custom-format snapshot, verified an OpenPGP AES-256 encrypt/decrypt checksum round trip, restored into a separate container, and compared all 26 snapshot tables by row count and content hash with 11/11 migrations. A unique recovery backend used the restored database on every platform node; the public route was temporarily cut over and the focused login/dashboard Playwright suite passed 6/6 before automatic rollback and cleanup. The final run measured a 2 ms on-demand snapshot boundary, 1.221 s dump, 6.338 s database restore, 43.470 s application readiness, and 52.053 s through the browser journey. The redacted manifest records only active route-referenced applications, topology, artifact digests, routes, and secret requirements. The live schema-3 PostgreSQL VM was 12,399 seconds behind after host suspend; schema 4 now requires boot-time and continuous Chrony, builds cleanly as `9249f950...`, and the builder refuses to overwrite a disk referenced by a live recorded service. The live VM was not destructively replaced, so the new image must be used for the next environment. Production KMS/HSM custody, off-host replication, immutable retention, and scheduled-backup RPO remain multi-system operator gates. |
| 2026-08-29 storage and resource pressure (Phase 7, Part 4) | PASS WITH A WASI CLI CONNECTION-LIFECYCLE LIMITATION | Disposable low-space, inode, read-only-root, CPU, memory, FD, and connection tests all produced bounded, observable behavior while peer OIDC traffic remained available. Inode-aware readiness, disk/inode/memory/process-FD metrics and alerts, exponential PID-1 restart backoff, one-restart VM sizing/read-only controls, an enforced WASI ResourceTable ceiling, full FD-denial error chains/counters, and identity-aware deployment verification were added. A request-scoped WASI HTTP canary held exactly 32 connections, denied the 33rd, exported 32 active during the hold, and returned to zero afterward. A 64-handle FD ceiling trapped only the offending request, incremented `wasm_policy_fd_denied_total`, and the next request passed. Persistent WASI CLI servers cannot observe individual socket destruction through the current Wasmtime address-check hook, so their connection counter is a conservative instance-lifetime budget; do not claim simultaneous-connection enforcement for that execution model until socket-drop interception or a process/cgroup boundary is implemented. Final rootfs `ce9f190a...` was rolled through all nodes with eBPF active, process FD usage was 144/1024 per node, focused Playwright passed 6/6, all platform targets were up, OIDC readiness was `database=ok`, no alert fired, and resource canaries were undeployed. |
| 2026-08-29 Phase 7 Part 4 source gates | PASS WITH DEPENDENCY NOTICE | WSL gates pass: formatting; required native workspace all-target check; workspace all-target Clippy with warnings denied; eBPF-feature node Clippy with warnings denied; 54 storage unit plus 12 integration tests; 64 runtime unit plus 7 integration tests; 11 metrics tests; 9 vm-testbed unit plus 3 doc tests; explicit release `wasm32-wasip2` builds for `hello-axum` and `http-hello-component`; and changed-script shell syntax. The focused OIDC Chromium suite passed 6/6. Rust still reports the separately tracked `proc-macro-error2 2.0.1` future-incompatibility notice. |

### Execution notes

- The workspace emits Rust's future-incompatibility notice for
  `proc-macro-error2 v2.0.1`; it does not fail Clippy or the current RustSec
  audit gate. Keep it visible until the dependency chain is removed or updated.
- The failed topology attempts did not persist the requested state file or start
  HAProxy/PostgreSQL/application deployment. Before retrying, verify that no
  failed-attempt Firecracker process or recorded network resource remains.
- `scripts/vm/build-postgres-rootfs.sh` cleanup was corrected after the initial
  image build: its `EXIT` trap now removes only its exact `mktemp` directory via
  `sudo`, avoiding a WSL permission error from PostgreSQL-owned chroot files.
  The resulting PostgreSQL image passed an ext4 read-only filesystem check.
- PostgreSQL image schema 3 deliberately pins every lifecycle command to the
  installed PostgreSQL 17 binary directory. Production image pipelines should
  apply the same rule: install one intended major version, validate all required
  executables during the image build, and avoid unversioned selector symlinks in
  PID 1 and migration automation.
- `build-all-images.sh` now checks a node-image boot-schema marker instead of
  treating filename presence as freshness. `provision-testbed.sh` independently
  rejects a missing or incompatible marker before creating any VM. The current
  node rootfs was rebuilt and its schema marker and filesystem were verified;
  topology provisioning is the next gate.
- The environment must remain up after deployment for the user's interactive
  browser testing. Teardown requires a later explicit request.
- The default application Playwright suite is not currently a safe load or
  release gate. Keep a small serial production-journey gate separate from
  parallel CRUD coverage until test data and selectors are isolated.
- A healthy OTLP receiver is only a prerequisite. The 2026-08-25 journey sent
  no spans or log records, so Phase 5 must remain failed until node subscriber
  wiring, bounded buffering, backend storage/query, trace propagation, and
  interruption/recovery behavior are proven.

## Phase 0: safety and prerequisites

- [x] Run all commands inside Linux or WSL2 from the repository root.
- [x] Confirm the target is a disposable local test environment.
- [x] Confirm no production credentials, certificates, customer data, or DNS
      zones are present.
- [x] Confirm KVM is accessible:

  ```bash
  test -r /dev/kvm && test -w /dev/kvm
  ```

- [x] Confirm sufficient free CPU, memory, and disk for three 2 GiB platform
      nodes plus NATS, PostgreSQL, builds, and observability.
- [x] Select one state file and use it for provisioning, deployment, inspection,
      and destruction:

  ```bash
  STATE_FILE=.prod-validation-single-host-state.json
  ```

- [x] Confirm the state file and its `.services.json` companion do not describe
      an existing environment. Inspect or explicitly destroy an old testbed
      before reusing its path.
- [ ] Record firewall, bridge, TAP, listening-port, and route state before the run.
- [x] Define the SLOs and concrete pass thresholds before generating traffic.

Suggested initial rehearsal thresholds, which must be replaced by product SLOs:

| Signal | Rehearsal threshold |
|---|---|
| Readiness | All required checks remain ready outside deliberate fault windows |
| HTTP error rate | Below 1% under expected test load |
| Recovery after one platform-node loss | Within 60 seconds |
| OIDC journey | Login, session validation, and logout all succeed |
| Telemetry loss | No sustained exporter or log-shipper drops |
| Backup restore | Restored application completes its database readiness check |

## Phase 1: release and configuration gates

- [x] Preserve unrelated worktree changes and record `git status --short`.
- [x] Confirm `rust-toolchain.toml` and `Cargo.lock` are committed.
- [x] Set build output on the Linux filesystem when the checkout is under `/mnt`:

  ```bash
  export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target
  ```

- [x] Run the required Rust checks:

  ```bash
  cargo fmt --all -- --check
  cargo check --workspace --all-targets \
    --exclude http-hello-component --exclude wasi-grpc-echo
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  cargo audit --deny warnings
  ```

- [x] Build every WASI application explicitly for `wasm32-wasip2`.
- [ ] Build the same locked release artifacts intended for promotion.
- [ ] Record SHA-256 checksums, provenance, dependency audit results, and SBOM
      location.
- [x] Review effective production-like configuration for unique node IDs,
      routable advertised addresses, memory/fuel limits, TLS expectations,
      secrets, log level, metrics, and eBPF mode.
- [ ] Confirm no placeholder or local-development credential can be promoted.

## Phase 2: provision the topology

Prepare Firecracker, kernel, and rootfs assets only when missing:

```bash
bash scripts/vm/build-all-images.sh
```

Provision the production-like topology:

```bash
bash scripts/vm/provision-testbed.sh \
  --preset production-like \
  --nodes 3 \
  --state-file "$STATE_FILE"
```

- [x] Record the NATS endpoint, every node admin/proxy address, and HAProxy URL.
- [x] Verify the recorded Firecracker PIDs belong to this state file.
- [x] Verify every platform node has a unique identity and writable filesystem.
- [x] Verify NATS connectivity and platform membership convergence.
- [x] Verify HAProxy exposes no node administration endpoint publicly.
- [x] Keep the state file and companion service state for the complete run.

## Phase 3: PostgreSQL and OpenID Connect application

Build and provision PostgreSQL when its image is absent:

```bash
bash scripts/vm/build-postgres-rootfs.sh
bash scripts/vm/provision-postgres-service.sh --state-file "$STATE_FILE"
```

- [x] Verify TCP readiness from the host.
- [ ] Verify PostgreSQL reachability from every platform-node network namespace,
      not only from the host.
- [x] Verify the database accepts only intended source networks and credentials.
- [x] Record PostgreSQL version, database name, migration version, and connection
      limits.

Deploy the local OpenID Connect WASI Hub checkout:

```bash
bash scripts/vm/deploy-oidc-hub-test.sh \
  --state-file "$STATE_FILE" \
  --app-dir /mnt/d/dev/openid_connect_wasi \
  --public-url http://127.0.0.1:8088
```

The deployment must not be accepted solely because an artifact upload or NATS
command succeeded.

- [x] Database migrations complete once and are safe to repeat.
- [x] Backend readiness reports the database as ready.
- [x] Frontend assets and SPA fallback routes load through HAProxy.
- [x] OIDC discovery returns the expected issuer.
- [x] Login reaches the identity flow without a 502 response.
- [x] Callback, token/session handling, authenticated API access, refresh where
      supported, and logout all work through the public origin.
- [x] Invalid credentials fail safely in the focused Playwright journey.
- [ ] Invalid redirect URIs, expired state/nonce, and expired
      sessions fail safely.
- [ ] No password, JWT, cookie, authorization header, database URL, or client
      secret appears in logs or traces.

Retain the environment for interactive browser testing. Do not destroy it until
the user explicitly finishes testing.

## Phase 4: metrics and alerting

The code currently exposes platform metrics at `/metrics`. Reconcile any
configuration or documentation that still refers to `/status/metrics` before
accepting this phase.

- [x] Deploy or connect a disposable Prometheus and Alertmanager.
- [x] Scrape every platform node separately over the intended private path.
- [x] Configure the actual authentication and TLS behavior of `/metrics`.
- [x] Add host, HAProxy, NATS, PostgreSQL, and OpenTelemetry Collector metrics.
- [x] Assign stable `environment`, `cluster`, `node`, `job`, and `instance` labels.
- [x] Run:

  ```bash
  promtool check config /path/to/prometheus.yml
  promtool check rules deploy/prometheus/*.yml
  ```

- [ ] Unit-test alert expressions using representative input series.
- [ ] Query every alert expression against the live test metrics to catch stale
      metric names and labels.
- [ ] Trigger and receive alerts for node down, application not ready, database
      failure, NATS failure, elevated HTTP errors, disk pressure, and telemetry
      exporter failure.
- [ ] Confirm alerts resolve and notifications are deduplicated after recovery.

## Phase 5: logs, traces, and audit records

- [ ] Emit structured JSON logs to stdout/journald.
- [ ] Deploy a host log agent such as Vector, Fluent Bit, or an OpenTelemetry
      Collector with a bounded disk buffer.
- [ ] Send application/platform logs and security audit records to distinct
      destinations or streams with different access and retention policies.
- [ ] Verify records contain timestamp, environment, node ID, application ID,
      instance ID, request ID, trace ID, severity, and component.
- [ ] Verify multiline failures remain parseable.
- [ ] Disconnect the log backend, fill the configured buffer to a safe test limit,
      restore connectivity, and confirm delivery without unbounded node memory.
- [ ] Alert on dropped records, saturated buffers, and unavailable audit sinks.

The `logging.otlp_endpoint` setting must not be accepted until the running node
actually integrates the OpenTelemetry layer; the presence of configuration alone
is not evidence of export.

- [ ] Export traces through a local OpenTelemetry Collector.
- [ ] Verify W3C trace context across HAProxy, platform proxy, frontend/backend
      routing, WASI HTTP calls, and PostgreSQL spans where instrumentation exists.
- [ ] Confirm one OIDC browser journey can be followed using a single correlated
      trace/request identity.
- [ ] Confirm exporter initialization returns a controlled error rather than
      panicking.
- [ ] Test collector interruption, bounded retry, recovery, and shutdown flushing.
- [ ] Confirm sampling retains errors and does not create uncontrolled cardinality.

## Phase 6: eBPF functional validation

Run this phase only on a kernel with the required BTF/eBPF support. The node rootfs
builder now enables the `ebpf` feature and packages all seven independently built
objects. Rebuild the image after running the pinned toolchain workflow; do not reuse
the pre-remediation image or mix feature sets within one test.

Source/build remediation completed after teardown:

```bash
bash scripts/ebpf/install-toolchain.sh
bash scripts/ebpf/build-ebpf.sh
CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target \
  cargo clippy -p node --features ebpf --all-targets -- -D warnings
```

The next microVM run must still prove the kernel-runtime portion below.

- [x] Record guest kernel version, configuration, BTF availability, capabilities,
      eBPF object checksum, and release feature set.
- [x] Confirm every eBPF object is packaged from a production-safe path.
- [x] Start in userspace fallback mode and record baseline metrics and overhead.
- [ ] Enable one probe group at a time.
- [x] Confirm `wasm_ebpf_active` reflects the actual mode.
- [ ] Confirm events are scoped to the intended platform cgroup/PID namespace and
      do not expose unrelated processes.
  - [x] **Part 1 — WASI HTTP application attribution:** each HTTP deployment runs
        on a dedicated single-thread executor whose Linux TID is registered in
        all three identity maps before application execution. Live OIDC frontend
        and backend registrations are distinct on every node, parsed syscall
        events preserve the TID, dispatch resolves the exact deployment, and
        security actions target that instance instead of an arbitrary process.
        Broader probe scoping remains open because block-I/O is still system-wide.
- [x] Generate file, TCP, memory-pressure, syscall, and block-I/O events and verify
      the corresponding metrics.
  - [x] **Part 2 — deterministic process, syscall, FD, TCP, and policy probes:**
        the canary produced identity-rich process start/exit, known-syscall,
        file open/close, TCP connect/accept/send/receive/close events and stable
        per-type Prometheus counters. An allowed PostgreSQL connection succeeded;
        a denied NATS connection failed before the host syscall and incremented
        `wasm_policy_connection_denied_total`.
  - [x] **Part 2B — deterministic non-fatal memory pressure:** a bounded page-cache
        plus anonymous-memory workload emitted exactly one `MEDIUM` event on the
        finalized object, incremented the stable Prometheus counter, set the
        pressure gauge to `1`, and disabled request acceptance without killing
        the node. After 30 quiet seconds the gauge returned to `0`, backpressure
        cleared, and OIDC remained ready through the other nodes.
  - [x] **Part 3 — block-I/O accuracy:** deterministic reads and writes now emit
        correctly classified records with device, sector, completed bytes,
        latency, issue PID/TID, cgroup ID, workload identity, and matching
        Prometheus counters. Buffered writeback is explicitly unregistered when
        the kernel worker—not the WASI executor—issues the block request.
- [x] Measure ring-buffer/event loss and dispatcher backpressure.
  - [x] **Part 4 — bounded pressure and loss accounting:** a deterministic FD
        burst filled the dispatcher queue and kernel ring, all loss was exported
        by bounded-label metrics, Prometheus and Alertmanager fired, memory stayed
        bounded, and the post-remediation burst preserved node/OIDC responsiveness.
- [x] Deny eBPF loading and confirm the node continues safely in fallback mode.
  - [x] **Part 5 — explicit failure and degraded modes:** optional monitoring
        failures keep the node ready in a reported degraded state and start the
        userspace fallback; mandatory monitoring failures refuse readiness.
        Monitoring-specific metrics and alerts distinguish these states from
        application/node unavailability, and clean restart restores active probes.
- [x] Restart every node and reload probes while preserving public application
      readiness; inspect bpffs and kernel identity-map residue explicitly.
  - [x] **Part 6 — lifecycle cleanup:** three deploy/stop/cold-start/remove cycles
        plus a rolling restart kept seven node-scoped programs attached, produced
        fresh application identities, and finished with zero active identities,
        tombstones, port bindings, kernel-map entries, or bpffs pins.
- [x] Unload/reload probes without leaving pinned maps or programs.
- [x] Compare request latency, CPU, memory, and context switches against the
      fallback baseline.
  - [x] **Part 7 — final-artifact A/B overhead:** four bounded rounds per mode
        alternated real capability-denied fallback and seven-program eBPF on the
        same node. All predefined acceptance limits passed with zero failed
        requests, parser errors, ring drops, or dispatcher saturations.
- [x] Repeat the production-representative application validation after the eBPF
      changes.
  - [x] **Part 8 — OIDC end-to-end revalidation:** deploy the OIDC frontend and
        backend, run PostgreSQL migrations, exercise the focused browser journey,
        verify exact application identities and representative event classes,
        roll every platform node, and repeat readiness, browser, eBPF, and
        observability gates.

### Part 1 record — per-application attribution for WASI HTTP

Status: **PASS on 2026-08-26 for the current in-process WASI HTTP execution
path.** This closes the shared-Tokio-worker attribution defect; it does not close
the remaining Phase 6 probe, pressure, failure-mode, cleanup, or overhead gates.

Implementation and safety properties:

- each HTTP instance owns one dedicated OS thread and a current-thread Tokio
  executor; listener, connection, request, Wasmtime, and WASI host work remain
  on that executor;
- the new thread reports its TID and blocks before constructing the executor;
  the supervisor registers the identity from its control thread, avoiding a
  self-observed `bpf(2)` syscall, and only then releases application execution;
- late eBPF map wiring uses interior mutability. The former `Arc::get_mut`
  silently skipped wiring after startup had cloned the supervisor;
- syscall counting, parsing, identity lookup, lifecycle storage, deregistration,
  and kill decisions all use the same TID key. The former PID/TID key mismatch
  made rate lookup unreliable;
- ordinary `socket`, `bind`, `listen`, and `connect` host syscalls are not treated
  as sandbox escapes. Wasmtime's `socket_addr_check` remains the authoritative
  allow/deny gate for WASI networking; and
- a security event targets `KillInstanceByTid`. Falling back to
  `KillLargestInstance` could terminate an unrelated deployment.

Validation evidence:

```text
node 0: oidc-admin TID 70; openid-connect backend TID 71
node 1: oidc-admin TID 68; openid-connect backend TID 69
node 2: openid-connect backend TID 68; oidc-admin TID 69
all registrations: namespace=oidc, map_count=3, exact versioned app ID
rootfs: sha256:30fe108dabda354d68e755a03cc0a299c8372f91d6a667d6bd239c5fa6975275
```

The image was canaried on node 0 and then rolled through nodes 1 and 2. After
each restart, direct node readiness and HAProxy readiness returned
`database=ok`. A controlled 20,000-request backend run left eBPF parse errors and
security violations at zero, produced no false network-control incident, and did
not churn either application instance. The focused Playwright login/dashboard
journey then passed 6/6 with one Chromium worker in 5.2 seconds.

Repository verification in WSL used the Linux target directory and passed:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets \
  --exclude http-hello-component --exclude wasi-grpc-echo
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p node --features ebpf --all-targets -- -D warnings
cargo test -p runtime
cargo test -p supervisor
cargo test -p ebpf-monitor --features ebpf
cargo test -p node --features ebpf
cargo build -p http-hello-component --target wasm32-wasip2
cargo build -p wasi-grpc-echo --target wasm32-wasip2
bash scripts/ebpf/build-ebpf.sh
```

Operational observations:

- `SKIP_EBPF_BUILD=true` means “copy the existing objects”; it does not verify
  that they are newer than their sources. One canary accidentally packaged the
  pre-fix `syscall_counter` object. Re-run `scripts/ebpf/build-ebpf.sh` before
  using that optimization and compare source/object timestamps or checksums.
- The anomaly threshold is intentionally 100,000 per CPU. Normal readiness
  traffic did not emit a rate event, even after the attribution load run. Part 2
  must provide a deterministic test configuration/workload and assert both the
  emitted event identity and Prometheus counter; do not lower the production
  threshold merely to make a test pass.
- A dedicated thread is an observable attribution boundary, not a security or
  resource-isolation boundary. Use one runtime process per tenant/workload, or a
  process-level cgroup whose cgroup ID is carried in eBPF events, when stronger
  blast-radius control is required. Standard process cgroups cannot distinguish
  two components inside the same `wasm-node` process.
- Libraries that create their own OS threads require explicit identity
  propagation or process isolation. The live result covers the current Wasmtime
  WASI HTTP path and the OIDC workload, whose async tasks stayed on the dedicated
  executor.

### Part 2 record — deterministic probe exercise

Status: **PARTIAL PASS on 2026-08-26.** Process lifecycle, known syscall, file
descriptor, TCP payload/lifecycle, namespace/application identity, runtime
network-policy, and deterministic non-fatal memory-pressure assertions passed.
Application-generated block-I/O evidence remains open for Part 3; therefore the
parent Phase 6 checkbox stays unchecked.

The reusable canary was `apps/hello-axum`, deployed directly on
`local-test-node-0` as an `ebpf-validation/ebpf-probe` workload with a `/tmp`
preopen, PostgreSQL `172.20.0.20/32` allowed, NATS `172.20.0.10/32` denied, and a
dedicated WASI HTTP executor. The probe endpoints are `/probe/file`,
`/probe/disk`, `/probe/memory`, `/probe/syscalls`, and `/call-raw`. The memory
endpoint remains bounded to 512 MiB by default; only an explicit
`EBPF_PROBE_MAX_MEMORY_MIB` deployment setting may raise it for a disposable
pressure rehearsal.

Passed evidence:

- a cold start emitted `process_start` and one deterministic
  `syscall_activity`; shutdown emitted `process_exit` after the executor thread
  had actually terminated. The exit retained `tid=77` and exact identity
  `ebpf-validation/ebpf-probe:p2b`, proving the short-lived identity tombstone
  covers asynchronous kernel exit delivery without keeping the TID active for
  enforcement;
- `/probe/file` returned `bytes=1048576`, `/probe/disk` returned
  `bytes=268435456`, and logs for the canary TID carried `FD opened` and
  `FD closed` with namespace, application, PID, TID, and FD fields;
- TCP connect, accept, send, receive, and close all appeared as stable
  `wasm_ebpf_events_by_type_total{event_type=...}` series. Structured records
  carried the exact canary identity, TID, FD/ports, and byte counts. A fresh
  corrected node snapshot recorded `tcp_connect=1`, `tcp_accept=16`,
  `tcp_send=33`, `tcp_receive=14`, `tcp_close=5`, `fd_open=20`, `fd_close=8`,
  `process_start=2`, `syscall_activity=2`, and zero parse errors;
- `/call-raw` to PostgreSQL returned `connected=true`. The same endpoint targeting
  NATS returned `connected=false` with `Permission denied`, and
  `wasm_policy_connection_denied_total` changed from `0` to `1`. This denial is
  correctly a runtime-policy event rather than an eBPF TCP-connect event because
  `socket_addr_check` rejects it before the kernel connect syscall; and
- normal connection closures had been misclassified as retransmits by treating
  `SYN_SENT -> CLOSE` as proof of retransmission. That heuristic was removed.
  The rebuilt object checksum is
  `312e2d79ccc3b6d8a4c9513465f9aa5459da8c9aad88b907109e7c5848988b93`;
  after restart and normal traffic, `wasm_ebpf_tcp_retransmits_total=0` and no
  `tcp_retransmit` per-type series was emitted. A future retransmit monitor must
  use a dedicated kernel retransmit tracepoint or compatible kprobe.

Part 2B memory-pressure evidence and production observations:

- the original `try_to_free_pages` kprobe was not deterministic for memory-cgroup
  reclaim. The finalized object also attaches
  `vmscan/mm_vmscan_direct_reclaim_begin`, retains the kprobe for compatibility,
  and uses one global ten-second timestamp map so paired hooks do not duplicate
  events. Direct reclaim is always classified `MEDIUM`; allocation order by
  itself is not evidence of a critical/OOM condition;
- the reusable WASI canary now has `/probe/cache` and `/probe/cache/clear` in
  addition to bounded `/probe/memory`. On a disposable 896-MiB node, a 350-MiB
  retained page-cache workload followed by a 250-MiB anonymous allocation
  completed successfully. Authenticated metrics reported exactly
  `wasm_ebpf_events_by_type_total{event_type="memory_pressure"}=1`,
  `wasm_ebpf_memory_pressure_level=1`, and no parser failure. Health reported
  `accepting_requests=false` while memory itself remained healthy at
  `437 MB / 862 MB`;
- after 30 seconds with no newer reclaim event, the fenced recovery task reset
  the gauge to `0`, cleared backpressure, and health reported
  `accepting_requests=true` at `191 MB / 862 MB`. The pressure counter remained
  `1`, as expected for a cumulative counter. OIDC readiness stayed
  `database=ok` through the HA front door;
- deliberately crossing the limit at 768/896 MiB also proved the new guest PID 1
  supervision model: the kernel could kill the `wasm-node` child, PID 1 stayed
  alive, restarted it after two seconds, and the two OIDC deployments converged
  without recreating the Firecracker VM. This is recovery evidence, not tenant
  isolation: production must still use process/cgroup isolation when one
  workload must not be able to restart the whole node runtime;
- `/health` now computes an effective memory ceiling from the configured process
  limit, guest `/proc/meminfo`, and cgroup-v2 `memory.max`, and exposes all three
  inputs in its message. Firecracker correctly reported each temporary size and
  the restored 2-GiB size. Production configuration should still set its process
  ceiling below actual VM/cgroup capacity rather than relying on the effective
  minimum;

Part 3 predecessor failure (retained to show why remediation was required):

- with a validation-only 1-ms slow-I/O threshold, boot activity produced two
  `block_io` events and the matching counter. The records had
  `io_type="unknown"`, PID/TID zero, and implausible device/sector fields. Four
  application requests each wrote and `sync_all`'d 256 MiB, yet the counter stayed
  `2 -> 2`. This is a failed deterministic attribution/correlation assertion and
  feeds directly into Part 3 tracepoint-layout and request-key remediation; and
- the normal 50-ms threshold and 2-GiB node size were restored. All three nodes
  were rolled to schema 3 and return HTTP 200. The finalized rootfs checksum is
  `d0545f3c547743dbe02aecac93683ab895af82fcf9684e1459c13c1f7995ae9c`
  and the kernel checksum is
  `7d6222139391f70bef3becb514916550c7b824e63a4e72ffa22485e6dab3e3d5`.
  HAProxy OIDC readiness again returned `database=ok`; the testbed remains live.

### Part 3 record — block-I/O accuracy

Status: **PASS WITH AN EXPLICIT ATTRIBUTION LIMITATION on 2026-08-26.** Event
layout, issue/completion correlation, read/write classification, byte accounting,
latency, device decoding, and Prometheus counters passed on the pinned Linux
6.1.80 guest kernel. The platform must not claim per-application ownership of
buffered writes that the filesystem later submits from a kernel writeback thread.

Root causes and corrections:

- Aya exposes the complete raw tracepoint record, including the eight-byte common
  header. The old parser incorrectly subtracted that header. It now reads the
  validated Linux 6.1 offsets: device at 8, sector at 16, issue bytes/completion
  error at 28, and `rwbs` at 32;
- sector alone is not a unique request key across devices or operations. The
  in-flight key is now device, sector, and operation, and an existing entry is
  not overwritten or double-counted;
- issue metadata now retains timestamp, PID, TID, cgroup ID, and submitted bytes.
  Completion exports actual bytes from `nr_sector * 512`, falling back to issued
  bytes for zero-sector operations;
- the tracepoint carries native kernel `dev_t`, decoded as 20 minor bits followed
  by the major number. The earlier userspace encoding formula produced the false
  device `0:65024`; the corrected records report the virtio root device `254:0`;
- operation parsing checks the first two `rwbs` characters, so a preflush-prefixed
  write such as `FW` is classified as write; and
- the event ABI change bumped the node-rootfs schema from 3 to 4 so provisioning
  rejects an image containing an incompatible userspace/BPF object pair.

Deterministic live evidence used a disposable `ebpf-validation/ebpf-io-probe:p3`
canary on node 0 with a validation-only one-millisecond slow-I/O threshold:

```text
before: block_io=204, read_bytes=8192, write_bytes=62427136, parse_errors=0
workload: POST-like GET /probe/disk?mib=256; returned bytes=268435456
after:  block_io=954, read_bytes=8192, write_bytes=301387776, parse_errors=0
delta:  750 slow events and 238,960,640 slow-write bytes
```

The counter intentionally includes only completions slower than the configured
threshold; it is not a whole-device byte counter, so its delta need not equal the
requested 256 MiB. Sample records had nonzero sectors, `bytes == nr_sector * 512`,
concrete latency, cgroup ID 1, and `dev=254:0`. Cold artifact reads retained
`namespace=ebpf-validation`, the exact versioned canary ID, PID/TID, and
`monitored_workload=true`. Buffered ext4 writes were submitted later by a kernel
writeback thread and therefore logged `app_id=<unregistered>` and
`monitored_workload=false`; this prevents false ownership.

For production-grade per-application write attribution, run each workload in a
dedicated runtime process/cgroup and carry the cgroup ID through writeback-aware
accounting. Standard cgroups cannot distinguish components that share the same
`wasm-node` process, and thread identity cannot follow buffered data after the
kernel writeback handoff. Direct-I/O or synchronous read evidence is useful for
probe correctness but is not a substitute for that isolation boundary.

Tracepoint field layouts can differ by kernel. Every promoted kernel must compare
`/sys/kernel/tracing/events/block/block_rq_issue/format` and
`block_rq_complete/format` with the parser contract, then repeat this test. A
mismatch must put monitoring into a visible degraded mode rather than silently
exporting plausible but incorrect fields; degraded-mode behavior remains Part 5.

After validation, the canary deployment and `ebpf-io.internal` route were removed.
The rootfs was restored to the normal 50-ms threshold and schema 4 was rolled one
node at a time. OIDC readiness returned `database=ok` after every replacement.
At Part 3 completion, live PIDs were node 0 `341140`, node 1 `341198`, and node
2 `341254`; NATS was PID `170213` and PostgreSQL PID `170523`. Part 3 artifact
checksums were:

```text
wasm-node-rootfs.ext4 deab03a267d30934d884935748f1d2a9f7c1eeab8255de335cab73999f3684d0
vmlinux-6.1             7d6222139391f70bef3becb514916550c7b824e63a4e72ffa22485e6dab3e3d5
```

### Part 4 record — ring-buffer pressure and event loss

Status: **PASS on 2026-08-26.** This part validates bounded buffering, explicit
kernel loss accounting, dispatcher saturation telemetry, alert delivery,
responsiveness, parser safety, and recovery. It does not set the acceptable
production overhead budget; that remains Part 7.

Implementation:

- all seven BPF objects expose a one-entry per-CPU `DROPPED_EVENTS` map. Failed
  `RingBuf::output` calls and failed reservations increment the local CPU value;
- the userspace consumer sums each monitor's per-CPU values, exports only the
  monotonic delta as
  `wasm_ebpf_ring_buffer_dropped_events_total{monitor=...}`, and separately
  counts map-read failures;
- the action channel remains bounded at 4,096 entries. Gauges expose configured
  capacity and current depth, while a counter records transitions into the full
  state. The consumer waits instead of allocating an unbounded overflow queue;
- Prometheus rules alert independently on kernel-ring loss, dispatcher
  saturation, and inability to read the kernel drop counter. All labels come
  from a fixed seven-monitor set; no application ID enters these metrics; and
- the reusable `hello-axum` validation workload adds
  `/probe/event-burst?iterations=N`. It opens and closes one small preopened file
  repeatedly, has a hard one-million-iteration bound, and reports the exact
  completed count without proportional memory allocation.

Failed assertions and remediation retained as production evidence:

- the first build appeared successful but the ELF lacked `DROPPED_EVENTS`.
  A host-mounted WSL timestamp allowed Cargo to reuse an older BPF object. The
  BPF builder now cleans only its isolated `bpfel-unknown-none` target before
  every build and explicitly unsets the shared native `CARGO_TARGET_DIR` for
  both clean and build. This prevents stale privileged instrumentation without
  deleting the workspace's native build cache;
- the first fully instrumented 100,000-iteration run completed in 5.116 seconds.
  It reported 87,292 `fd_watcher` ring drops, 19 dispatcher saturation
  transitions, zero parse errors, and eventual queue recovery from 4,096 to a
  low background depth. A second 50,000-iteration run raised the drop counter
  to 126,131 and saturation counter to 65. Prometheus and Alertmanager both
  fired `EbpfRingBufferEventsDropped` and `EbpfDispatchQueueSaturated`;
- ordinary FD and TCP success events were logged at info level, amplifying the
  burst enough to cause a transient Prometheus scrape outage even though OIDC
  remained ready through the other nodes. These high-cardinality success records
  now log at debug; warning/error events and aggregate metrics remain visible;
- a 500,000-iteration attempt exceeded the workload's request fuel and did not
  return a completed-count response. It is excluded from pass evidence. The
  responsiveness harness now treats any non-2xx workload result as failure; and
- the finalized 100,000-iteration run returned
  `completed=100000`. Ten health samples taken during execution recorded zero
  node failures and zero OIDC failures. The queue stayed below capacity with
  zero saturation transitions, parse errors remained zero, and processed events
  increased from the pre-run value by 100,752. Firecracker RSS rose only from
  829,748 KiB to 887,436 KiB, while guest health reported 576 MiB used of its
  effective 1,995-MiB ceiling and continued accepting requests.

The alert rules loaded with evaluation health `ok`. The pressure canary and
`ebpf-pressure.internal` route were removed. The final rootfs was rolled one
node at a time; OIDC database readiness stayed healthy. A final node-0 restart
cleared the undeployed canary's stale health series, after which Prometheus had
no active alert. Final PIDs are node 0 `387278`, node 1 `368191`, node 2
`368273`, NATS `170213`, and PostgreSQL
`170523`. Final artifact checksums are:

```text
wasm-node-rootfs.ext4 a8ac95c1364f3313da92373d49050a526c38be3d99b0e68563aa2c020bce328f
vmlinux-6.1             7d6222139391f70bef3becb514916550c7b824e63a4e72ffa22485e6dab3e3d5
```

### Part 5 record — failure and degraded modes

Status: **PASS on 2026-08-27.** The test targeted only recorded node
`local-test-node-0` (`172.20.0.12`) in a fresh three-node production-like
topology. Nodes 1 and 2, the separate NATS VM, HAProxy, and the disposable
observability services remained running. The routed `default/ebpf-failure:v1`
canary was checked through `http://127.0.0.1:8088/health` with host
`ebpf-failure.internal` after every transition.

The reusable command was:

```bash
bash scripts/vm/validate-ebpf-degraded-modes.sh \
  --state-file .prod-validation-single-host-state.json
```

The runner resolves the exact node from state and uses state-driven
`restart-node`; it does not kill processes, remove devices, or select VMs by a
broad pattern. Local fault hooks are accepted only through explicit testbed boot
arguments and must never be configured in production.

| Condition | Mechanism | Optional-mode result | Recovery / alert evidence |
|---|---|---|---|
| Missing eBPF capability | Real guest execution with only `CAP_BPF`, `CAP_SYS_ADMIN`, `CAP_PERFMON`, and `CAP_NET_ADMIN` removed | HTTP 200 degraded readiness; `ebpf_active=0`; reason `missing_capability`; application and peers remained available | Clean restart restored active monitoring |
| Insufficient privilege | Deterministic preflight fault | Fallback active; reason `insufficient_privileges` | Metrics, readiness, canary, peers, and clean restart passed |
| Program rejected | Deterministic loader fault | Fallback active; reason `program_load_rejected` | Metrics, readiness, canary, peers, and clean restart passed |
| Tracepoint/probe unavailable | Deterministically skipped disk monitor while other objects loaded | eBPF remained active but explicitly degraded with reason `partial_probe_set` | `EbpfMonitoringIncomplete` rule is distinct from total monitoring loss; clean restart restored the full probe set |
| Missing/malformed BTF | Disposable copy of the real kernel with `.BTF` and `.BTF_ids` removed by `objcopy` | Guest booted; real relocations/verifier load failed; fallback active; reason `missing_btf` | `EbpfMonitoringUnavailable` fired; `PlatformNodeDown` did not; application and peers remained available |
| Ring consumer termination | Deterministic consumer exit after startup | Watchdog changed active to false, reason to `consumer_exited`, and started fallback | Metrics/readiness/canary passed; clean restart restored the consumer |
| Mandatory monitoring plus missing BTF | Same physically stripped kernel with `[ebpf].required=true` | Node deliberately did not become ready; nodes 1 and 2 continued serving the canary | A final normal-kernel restart restored node 0 and active monitoring |

Two test defects produced production-relevant observations:

- removing *all* Linux capabilities also removed `CAP_DAC_OVERRIDE`, preventing
  the root process from opening its cloned redb state file. Capability tests and
  production units must remove only the eBPF-related capabilities or grant the
  state directory to the runtime identity. Least privilege must be validated
  against every required filesystem and network operation, not only BPF load;
- node readiness can return before a redeployed application has reconverged on
  that node. During early iterations HAProxy briefly returned 502 after a clean
  node restart. Production load balancers need application-aware backend health,
  scheduler convergence/startup gates, and connection draining; node-level
  `/readyz` alone is insufficient for an application pool.

Implementation added a dynamic monitoring availability state, bounded failure
reasons, `wasm_ebpf_monitoring_required`,
`wasm_ebpf_monitoring_degraded`, and
`wasm_ebpf_monitoring_failures_total{reason}`. Optional failures are a degraded
dependency and mandatory failures are unhealthy. Loader failures retain their
stage so a real malformed/missing-BTF verifier path is not collapsed into a
generic unavailable reason. Prometheus has separate unavailable and incomplete
eBPF alerts, explicitly distinguishing monitoring failure from application
failure.

Final live evidence after the clean restart:

- NATS PID `52250`; node PIDs `90300`, `65543`, and `65596`; every platform node
  returns HTTP 200 and all three Prometheus platform-node targets are up;
- node 0 reports `ebpf_active=true`, `monitoring_required=false`,
  `monitoring_degraded=false`, and no degraded reason;
- kernel SHA-256 is
  `7d6222139391f70bef3becb514916550c7b824e63a4e72ffa22485e6dab3e3d5`;
  node-rootfs SHA-256 is
  `aaaf63025213b3cedb716b52e9111334d563b695bb78976fd4aee4165dd7b89b`;
- during Part 5, the HAProxy exporter target was down and
  `HAProxyExporterDown` was active. The pre-Part-6 remediation added the
  loopback-only native exporter to canonical topology generation, reloaded only
  the state-recorded HAProxy process, and proved the Prometheus target `up`.

The final WSL source gate passed formatting, required native workspace checking,
workspace Clippy with warnings denied, eBPF-feature node Clippy, 120 eBPF-monitor
tests, vm-testbed unit/doc tests, all seven BPF builds, changed-script syntax, and
release `wasm32-wasip2` builds for `hello-axum`, `http-hello-component`, and
`wasi-grpc-echo`. `cargo audit --deny warnings` initially rejected yanked
`chacha20 0.10.1`; the lockfile was updated to non-yanked `0.10.2`, after which
the audit and native workspace recheck passed. The known
`proc-macro-error2 2.0.1` future-incompatibility notice remains.

The environment remains live in `.prod-validation-single-host-state.json` for
operator inspection and the next explicitly authorized part. Teardown has not
been run.

### Part 6 record — application and eBPF lifecycle cleanup

Status: **PASS on 2026-08-27.** The reusable validator is:

```bash
bash scripts/vm/validate-ebpf-lifecycle-cleanup.sh \
  --state-file .prod-validation-single-host-state.json \
  --cycles 3
```

It deploys the repository's `wasi:http/incoming-handler` fixture rather than a
CLI component that runs its own permanent server. This distinction matters: an
early test used `hello-axum`, whose long-lived `wasi:cli/run` call correctly hit
the runtime's 30-second epoch ceiling. Production HTTP services should use the
incoming-handler model, where the ceiling applies to each request; a permanent
CLI-style server must not be deployed until the platform has a separately
designed service-lifetime and request-deadline model.

The final run completed all of the following on `local-test-node-0`, `-1`, and
`-2`:

- three complete deploy, warm, forced-stop, cold-start, and remove cycles for
  `default/ebpf-lifecycle:v1`;
- a rolling restart of all three nodes during cycle 1 while the application
  remained reachable directly and through the HAProxy front door;
- a different `tid:registered_at_ns` identity after every forced stop,
  redeploy, and node replacement;
- exact agreement between active identities and all five kernel identity maps;
- route removal on every node, deletion of per-application Prometheus series,
  and rejection of requests to the removed route;
- no new eBPF action record attributed to the removed identity; and
- expiry of the bounded 30-second tombstone retention window.

The test exposed and closed several independent lifecycle defects:

1. `RemoveApp` left its route in storage/router state, allowing a request to
   cold-start an application after undeploy. Routes are now removed before
   instance shutdown.
2. The supervisor retained an empty application pool, so health and Prometheus
   continued reporting an undeployed app. The empty live pool is now removed,
   and absent label values are deleted from both application gauges.
3. Grace-period artifacts were indistinguishable from deployed state during
   restart and peer bootstrap. Storage now exposes `list_deployed_apps()`;
   local restore and bootstrap configs/routes exclude undeploy-marked apps,
   while the artifacts remain recoverable until GC. Redeploy clears the marker.
4. A short-lived CLI could print `Undeploy requested` and exit before its
   buffered NATS publish reached the server. `NatsBus::publish()` now flushes
   before returning, so success means the control message reached NATS.
5. The canonical application deployment helper assumed every Wasm package had
   a binary target. It now resolves either a binary or `cdylib`, enabling direct
   production-like validation of WASI HTTP components.
6. The original HAProxy topology omitted the native Prometheus listener even
   though observability always scraped it. Canonical HAProxy generation now
   binds metrics to `127.0.0.1:8405`; the state-driven reload helper validates
   the exact recorded config/process and updates lifecycle state atomically.

Final live evidence:

- NATS PID `52250`; HAProxy PID `99344`; node PIDs `171971`, `172045`, and
  `172120`; every node process is alive and its health endpoint returns 200;
- every node reports `ebpf_active=true`, seven attached programs, and no
  monitoring degradation;
- every `/admin/ebpf/identities` response reports `active_tids=[]`,
  `recent_tombstones=0`, `port_bindings=0`, five zero-valued kernel maps,
  `bpffs_mounted=true`, and `pinned_entries=0`;
- Prometheus reports the HAProxy target up and `/api/v1/alerts` returns an empty
  alert set;
- final rootfs SHA-256 is
  `89b06132006d44535f6c0f38c356707d7023f4e168a1beba697ad3972b97cbe5`;
  kernel SHA-256 remains
  `7d6222139391f70bef3becb514916550c7b824e63a4e72ffa22485e6dab3e3d5`.

Repository verification passed in WSL with
`CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target`: formatting, the required
native workspace all-target check, workspace Clippy with warnings denied,
eBPF-feature node Clippy, storage (51 unit and 12 integration), messaging (13),
metrics (11), supervisor (38 unit and 10 integration), and eBPF monitor (121)
tests. Release `wasm32-wasip2` builds passed for `http-hello-component` and
`wasi-grpc-echo`; changed-script syntax and `cargo audit --deny warnings` also
pass. The `proc-macro-error2 2.0.1` future-incompatibility notice remains a
separately tracked dependency warning.

The environment remains live and contains no deployed lifecycle fixture.
Teardown has not been run.

### Part 7 record — eBPF overhead

Status: **PASS on 2026-08-29 against the final guest image.** The reusable
validator and exact final command are:

```bash
CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target \
  bash scripts/vm/validate-ebpf-overhead.sh \
    --state-file .prod-validation-single-host-state.json \
    --requests 20000 \
    --concurrency 32 \
    --blocks 2 \
    --rounds-per-block 2 \
    --output /tmp/phase6-part7-ebpf-overhead-final-v3.json
```

The runner fixes its acceptance criteria before load begins: at most 5%
throughput degradation; a p99 increase no larger than the greater of 5% or one
millisecond; at most 15% more node/application CPU per 1,000 requests; at most
25% more node/application context switches per 1,000 requests; at most 64 MiB
additional peak RSS; and zero request failures, parse errors, kernel ring drops,
or dispatcher saturation transitions. Active mode must also emit real eBPF
events.

Methodology:

- each of two blocks restarted the same platform node first with the real eBPF
  capabilities removed and then with all capabilities restored;
- each mode ran two 20,000-request samples at concurrency 32, for four samples
  and 80,000 requests per mode (160,000 requests total);
- a dedicated `wasi:http/incoming-handler` canary ran on node 0, with an explicit
  10,000 request/s application limit so platform rate limiting did not distort
  the benchmark;
- the runner sampled latency percentiles and throughput, aggregate node CPU/RSS
  and task context switches from procfs, dedicated application-TID CPU/context
  switches, and Prometheus eBPF event/loss/parser/queue counters; and
- the modes were alternated rather than measured in one long pair to reduce
  host thermal, cache, and background-load bias.

Final results:

| Measure | Userspace fallback | Active eBPF | Change / gate |
|---|---:|---:|---:|
| Median requests/s | 344.88 | 344.46 | 0.12% loss; <= 5% |
| Median p99 | 120.00 ms | 121.12 ms | +1.11 ms; <= 6.00 ms allowance |
| Node CPU / 1,000 requests | 3.821 s | 3.886 s | +1.72%; <= 15% |
| Application-TID CPU / 1,000 requests | 0.743 s | 0.765 s | +2.95%; <= 15% |
| Node context switches / 1,000 requests | 3,496.29 | 3,915.68 | +12.00%; <= 25% |
| Application context switches / 1,000 requests | 594.45 | 596.34 | +0.32%; <= 25% |
| Peak node RSS | 50.46 MiB | 59.08 MiB | +8.62 MiB; <= 64 MiB |
| Failed requests | 0 / 80,000 | 0 / 80,000 | zero required |
| eBPF events | 0 | 170,842 | active evidence present |
| Parse errors / ring drops / queue saturations | 0 / 0 / 0 | 0 / 0 / 0 | zero required |

Application CPU and context-switch accounting is exact for the dedicated WASI
executor TID. Per-application RSS cannot be isolated while several applications
share the `wasm-node` address space, so RSS is conservatively measured for the
whole node process. Production requiring tenant-level memory attribution must
give each workload a process/cgroup boundary; this is the same stronger boundary
already recorded for buffered-write attribution.

The run exposed and closed the following correctness/tooling gaps before the
final measurement:

1. Application rate-limit settings were persisted but not installed in the
   node-local limiter. Deploy/config-update/remove now apply and clear overrides.
   Per-IP buckets are keyed by application and IP, and config replacement clears
   stale buckets, preventing cross-application throttling and stale limits.
2. Procfs node context-switch sampling originally read only the process leader;
   it now aggregates every task under `/proc/self/task`.
3. Slow block-I/O set action-level `degraded_mode` permanently. Slow-I/O and
   critical-memory causes are now tracked independently. One generation-fenced
   recovery worker per node extends the 30-second quiet window for new events,
   clears only the slow-I/O cause, and exits without leaking threads. A LOW
   memory event clears only the memory-pressure cause. A 1,000-event unit burst
   proves that only one worker is active.
4. Hot-reloaded eBPF thresholds updated userspace dispatch state but not the
   programs' kernel `CONFIG` maps. The loader now retains owned handles for every
   monitor CONFIG map and updates all of them from the node config-sync loop.
5. The canonical deployment script converted Cargo binary target hyphens to
   underscores and rejected a successfully built artifact. Binary names are now
   preserved; underscore normalization remains limited to library/cdylib output.

The recovery path was proved in the final Firecracker guest at the established
validation-only 1-ms slow-I/O threshold. A 256-MiB synchronous canary operation
returned HTTP 200, produced 1,259 events before the immediate status sample, and
set `degraded_mode=true`. Delayed filesystem writeback continued producing slow
events, so the single worker correctly extended rather than prematurely cleared
the incident. After the normal 50-ms threshold was restored, the count stabilized
at 1,353 and the worker returned the node to `degraded_mode=false` after a real
quiet window while eBPF remained active and parse errors stayed at zero. A
separate 1-ns
experiment correctly proved live kernel-map update but generated an unreasonable
system-wide event storm and triggered the explicit `consumer_exited` degraded
mode; operators must reject near-zero thresholds through deployment policy and
must use the ring-pressure test rather than such a setting for capacity work.

The corrected image was rolled through all three nodes. Final node-rootfs
SHA-256 is
`3f57e9c2bc0cc80d8c95d7e30e048cd8447c641098391e3dbf782f3a4f4da217`;
kernel SHA-256 remains
`7d6222139391f70bef3becb514916550c7b824e63a4e72ffa22485e6dab3e3d5`.
After the final A/B run, node 0 was active again, the benchmark deployment and
route were removed, NATS remained connected, all three node health checks were
HTTP 200, and every node reported seven attached programs with no monitoring or
action degradation. Final node PIDs were `162893`, `156195`, and `156244`; node
0 reported zero active identities, tombstones, port bindings, kernel-map entries,
or bpffs pins.
Prometheus reported all 10 targets up and no alert was active.

Final WSL gates pass: formatting; required native workspace all-target checking;
workspace all-target Clippy with warnings denied; eBPF-feature node checking;
126 eBPF-monitor tests; 13 focused proxy limiter tests; 29 node library and 71
node binary tests; nine vm-testbed unit tests and three doc tests; explicit
release `wasm32-wasip2` builds for `http-hello-component` and
`wasi-grpc-echo`; changed-script syntax; and `cargo audit --deny warnings` over
749 dependencies. Cargo still prints the separately tracked
`proc-macro-error2 2.0.1` future-incompatibility notice.

The environment remains live for operator inspection; teardown has not been run.

### Part 8 record — OIDC application revalidation

Status: **PASS on 2026-08-29 for the production-representative application
gate, with the documented process/cgroup and capacity limits.** This completes
the eight planned Phase 6 parts. It does not turn the single-host rehearsal into
production approval: system-wide block-I/O scope, shared-process RSS, production
capability reduction, and real storage sizing remain explicit operator gates.

The canonical OIDC deployment workflow rebuilt the locked frontend and both
`wasm32-wasip2` components, audited 101 npm packages with no reported
vulnerability, provisioned PostgreSQL 17.11, applied migrations through V37,
and completed the repeatable seed. HAProxy served the frontend and backend on
the same public origin. Backend readiness returned:

```json
{"checks":{"database":"ok"},"status":"ready"}
```

The deployed identities were:

```text
frontend: oidc/oidc-admin-wasi:vd2d032b3c42d
backend:  oidc/openid-connect-wasi:vb055dfbdf0ae
```

The focused Playwright Chromium gate used one worker against
`http://localhost:8088`. Before the rolling restart, all six login/dashboard
tests passed in 5.2 seconds: form rendering, invalid credentials, successful
login and redirect, empty-field validation, authenticated navigation, and
API-backed statistics. The same six tests passed after the rolling restart in
4.9 seconds. This exercised frontend asset delivery, backend HTTP traffic,
authentication, database reads/writes, and negative authentication behavior.

Runtime evidence tied connections to `172.20.0.20:5432` to the exact versioned
backend identity rather than the whole `wasm-node` process. Process-start and
known-syscall records also carried the corresponding frontend/backend TIDs.
Prometheus observed process/syscall, FD open/close, TCP
connect/accept/send/receive/close, and block-I/O event classes. After the final
browser run the three nodes collectively reported 6,081 processed events,
including 598 FD opens, eight TCP connects, and two slow block-I/O completions,
with zero parse errors, monitoring-degraded gauges, or dispatcher saturation
transitions. This is application-path validation; Part 2 and Part 3 remain the
deterministic content/accuracy proofs for every individual probe field.

Nodes were replaced one at a time while PostgreSQL, NATS, HAProxy, and the
observability services stayed running:

```text
local-test-node-0 -> Firecracker PID 171298
local-test-node-1 -> Firecracker PID 171366
local-test-node-2 -> Firecracker PID 171434
```

After each replacement, the node accepted requests, NATS was connected, all
seven eBPF programs were attached, both OIDC applications were serving, public
frontend delivery succeeded, and backend readiness remained `database=ok`.
Final fresh application TIDs were node 0 backend `73` / frontend `75`, node 1
frontend `74` / backend `76`, and node 2 frontend `74` / backend `76`. Each node
reported exactly two entries in each of the five kernel identity maps, zero
tombstones, zero stale port bindings, and zero bpffs pins. Reuse of a numeric TID
on a different freshly booted VM is normal; identity freshness is established by
the new Firecracker process, boot-relative registration timestamp, exact
deployment ID, and clean map cardinality.

Final service gates were 10/10 Prometheus targets up, no active alert, three
nodes with `wasm_ebpf_active=1`, and
`wasm_ebpf_monitoring_degraded=0`. All eBPF action degradation cleared after the
normal slow-I/O quiet window. The platform nodes still report overall health as
degraded because their disposable 2-GiB root filesystems have about 1,579 MiB
free and cross the configured percentage-based low-disk warning. They remain
startup-complete and accepting traffic, but production sizing must provide a
separate durable data strategy, growth headroom, and an alert threshold aligned
with the selected disk size rather than accepting this warning.

Security note: the local development seeder prints disposable credentials and
a generated API key. They were not copied into this record. Production seeding
must use an external secret manager and must never emit secrets into build,
migration, or CI logs.

Post-change repository verification passed in WSL with
`CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target`: formatting, the required
native workspace all-target check, workspace Clippy with warnings denied,
eBPF-feature node Clippy with warnings denied, 87 common tests, 119 eBPF-monitor
tests, 64 runtime unit plus 7 integration tests, 38 supervisor unit plus 10
integration tests, and 98 node library/binary unit plus 10 integration tests.
Explicit `wasm32-wasip2` builds passed for the probe and both workspace WASI
examples, as did all seven BPF object builds, changed-script shell syntax, and
scoped diff whitespace checks. The repository-wide whitespace check remains
noisy because unrelated dirty-worktree files retain CRLF changes; those files
were preserved.
Part 4 additionally exercised the clean BPF rebuild path while the shared native
target variable was set, proving the script confines cleanup to the isolated BPF
target.
The separately tracked `proc-macro-error2 2.0.1` future-incompatibility notice
still appears.

Do not pass this phase while probes remain system-wide, object paths are tied to
a development checkout, unsafe event parsing is unvalidated, or the primary node
retains broad capabilities unnecessarily.

## Phase 7: controlled failure tests

Run one fault at a time first, then selected combinations. Always record the exact
target from the state file; never kill processes or remove networking by broad
pattern matching.

### Platform node

- [x] Gracefully drain and stop one platform node.
- [x] Abruptly terminate one platform-node Firecracker process.
- [x] Pause or isolate one node long enough to trigger health removal.
- [x] Verify HAProxy stops routing to the unhealthy application pool.
- [x] Verify remaining capacity stays within the declared Phase 7 availability
      SLO. Higher-rate capacity remains Phase 8.
- [x] Restore or rebuild the node and verify membership and deployment convergence.

### Part 1 record — controlled platform-node drain and N−1 availability

Status: **PASS on 2026-08-29 after closing the missing operator-drain path.**
The Phase 7 availability SLO is deliberately below configured admission limits:
20 backend-readiness requests paced at 4 requests/s, zero non-2xx responses, and
maximum request time below one second while one of three platform nodes is
absent. This is an availability gate, not the Phase 8 capacity envelope.

The first orderly Firecracker `SendCtrlAltDel` experiment exposed a real gap.
The guest rebooted and the VMM exited, but the serial log ended at
`reboot: machine restart` without the platform's `NodeDraining` flow. The
runtime already handled that event, but `wasm-ctl` offered no operator command
to publish it and the handler did not fence new traffic before waiting for the
application drain.

Remediation:

- `wasm-ctl node --target NODE drain --timeout-secs N` publishes the existing
  targeted `NodeDraining` control event;
- the command requires an explicit target and refuses an ambiguous drain;
- the target node immediately sets its shared request gate to rejecting, so its
  readiness endpoint becomes HTTP 503 and HAProxy stops assigning new work;
- the existing supervisor drain then shuts down application instances; and
- operators stop or replace the VM only after readiness is false and active
  instances reach zero.

The live canary targeted only `local-test-node-0`. Readiness changed to HTTP 503
with `accepting_requests=false` while its two instances were still completing
the drain. Both frontend and backend then reached zero instances, and direct
application traffic on node 0 returned HTTP 503. HAProxy marked node 0 `DOWN`
in both OIDC pools with `L7STS/503`; public backend readiness remained
`database=ok` through nodes 1 and 2. The guest was then stopped through its exact
Firecracker control socket, with no broad process matching.

With node 0 absent, all 20 paced requests passed:

```text
request rate: 4 requests/s
HTTP status: 20/20 HTTP 200
minimum: 32.884 ms
maximum: 52.384 ms
acceptance gate: zero failures and maximum < 1,000 ms
```

An earlier unpaced 30-request, concurrency-2 burst returned 29 HTTP 200 and one
HTTP 429 from a surviving node. HAProxy had already removed node 0 and no 5xx
occurred. This is important production evidence: the front door allows a larger
burst than the node-local per-IP limiter can guarantee after capacity drops from
three nodes to two. Phase 8 must define the intended client/request mix and
either align both limiter layers for N−1 capacity or use an explicitly designed
distributed limiter. Availability tests must not misclassify expected 429
admission control as backend failure, but production SLOs must decide whether
429 counts against the user-visible error budget.

The state-driven restart rebuilt node 0 as Firecracker PID `190287` with both
OIDC deployments and two fresh eBPF identities. The corrected immutable image
was then rolled through nodes 1 and 2 as PIDs `190354` and `190405`. Final node
rootfs SHA-256 is
`b6512b8bda8ad8dbe17e5b80d16d8414e535fbe0579ecdb269fb97b7b69c4619`.
All nodes accept requests, connect to NATS, serve both deployments, and report
active eBPF monitoring. The focused Playwright login/dashboard suite passed 6/6
in 4.2 seconds, Prometheus reported all 10 targets up, and no alert remained.

WSL source gates passed with the Linux target directory: formatting; required
native workspace all-target checking; workspace all-target Clippy with warnings
denied; eBPF-feature node Clippy; 23 `ctl` tests; and the focused node drain
regression in both node library and binary targets. Cargo continues to print the
separately tracked `proc-macro-error2 2.0.1` future-incompatibility notice.

### Network

- [x] Add latency, jitter, packet loss, and bandwidth restriction to one node.
- [x] Partition one platform node from NATS.
- [x] Partition one platform node from PostgreSQL.
- [x] Verify timeouts are bounded and recovery does not cause a retry storm.
- [x] Verify stale routes or application instances are not advertised indefinitely.

### Part 2 record - controlled network and dependency faults

Status: **PASS on 2026-08-29 after closing bounded-readiness and
superseded-deployment gaps.** Each fault targeted the exact node-0 address and
TAP recorded in `.prod-validation-single-host-state.json`. Only one fault was
active at a time, every rollback used the same full selector used to create the
fault, and the final audit found no Phase 7 firewall rule or netem qdisc left
behind.

The node-0 TAP `tap-7976f61b7d5` was configured with 100 ms latency, 20 ms
jitter, 5% packet loss, and a 20-Mbit/s rate. A 50-packet probe observed 45
replies, 10% sample loss, and 74.065/101.395/143.288 ms minimum/average/maximum
round-trip time. Twelve direct readiness requests all returned HTTP 200 in
181-258 ms. The qdisc recorded 241 packets and 14 drops before it was restored
to `fq_codel`; all three final TAPs report `fq_codel`.

An exact `FORWARD` rule then dropped node 0 (`172.20.0.12`) traffic to NATS
(`172.20.0.10:4222`). The rule counted 17 packets and 3,563 bytes. Node 0
returned HTTP 503-ready within 16 seconds with NATS explicitly disconnected,
while node 1 and the public OIDC database path remained healthy. Its serial log
contained one transition into degraded mode and no reconnect loop. Removing
the exact rule restored NATS health within 12 seconds and produced one recovery
transition.

The first equivalent PostgreSQL partition exposed a production-significant
application defect. A node-0 backend readiness request returned an initial 502
in 2.043 seconds, then a later request remained blocked beyond 47 seconds; an
independently bounded client timed out after 5.010 seconds. The platform proxy
had established the WASI HTTP request, but the backend's PostgreSQL connection
had no application-level readiness deadline. HAProxy's `timeout server 60s`
therefore could not guarantee prompt pool withdrawal. The independent frontend
on node 0 stayed HTTP 200, and HAProxy continued serving backend readiness from
the other two nodes.

The OIDC backend at
`D:/dev/openid_connect_wasi/crates/openid-connect-wasi/src/router/health.rs`
now races the complete database connect/query operation against a two-second
deadline on both native Tokio and `wasm32-wasip2`. A timeout returns the
existing not-ready response instead of trapping or hanging. Two regression
tests cover completion and cancellation; the application passed formatting,
17 backend tests, all-target Clippy with warnings denied, and a release
`wasm32-wasip2` build.

After deploying backend `oidc/openid-connect-wasi:vd5fc09739b3d`, the repeated
partition behaved as intended. The first direct node-0 check returned
`{"checks":{"database":"failed"},"status":"not_ready"}` with HTTP 503 in
2.013 seconds. The platform circuit breaker returned the next two HTTP 503
responses in 9.4 and 4.2 ms. Node-0 frontend traffic remained HTTP 200 in 3.7
ms, public backend readiness remained HTTP 200 in 35 ms, and HAProxy marked
only `oidc_backend_nodes/node0` down with `L7STS/503`, leaving two active
servers. The rule counted 17 packets and 1,020 bytes. Timeout warnings occurred
at the bounded four-second health cadence rather than as an uncontrolled retry
loop; the node stayed responsive and its Firecracker RSS remained bounded.
After exact rule removal, the circuit breaker completed its roughly 30-second
cooldown, a direct readiness probe returned HTTP 200 in 37 ms, and HAProxy
automatically restored node 0 with three active servers and an empty queue.

The final telemetry gate found three stale `ApplicationNotReady` alerts for the
superseded backend version even though the replacement was healthy. The alert
correctly reflected old zero-instance metric labels: the OIDC deployment
workflow had deployed a new content-addressed version without removing the old
desired-state version. `scripts/vm/deploy-oidc-hub-test.sh` now removes only
older versions of its two known OIDC components after the replacements pass
their health checks and waits until all node metric endpoints stop exporting
those labels. Separately, `/admin/apps` now uses `list_deployed_apps()`, so
recoverable undeploy-grace artifacts remain available to GC without being
presented as live deployments. A complete repeat deployment passed this cleanup
gate.

The corrected node image SHA-256 is
`e23eeac8baaaa1b8df81174861870a373adf4663326d6f56bd84ebf07b94c246`.
It was rolled one node at a time as PIDs `226513`, `226584`, and `226655`.
Public OIDC readiness remained HTTP 200 during each replacement. Every node now
lists exactly frontend `oidc/oidc-admin-wasi:vd2d032b3c42d` and backend
`oidc/openid-connect-wasi:vd5fc09739b3d`, each with one instance. All three
HAProxy backend members are up, PostgreSQL readiness is `database=ok`, the
focused single-worker Playwright login/dashboard gate passes 6/6 in 5.3
seconds, Prometheus reports `sum(up)=10`, and no alert remains.

WSL source gates passed with the Linux target directory: formatting; the
required native workspace all-target check; workspace all-target Clippy with
warnings denied; 51 storage unit plus 12 integration tests; 30 node library
tests; explicit `wasm32-wasip2` builds for both workspace WASI applications;
`cargo audit --deny warnings` across 749 locked dependencies; changed-script
syntax; and scoped whitespace checks. Cargo continues to emit
the separately tracked `proc-macro-error2 2.0.1` future-incompatibility notice.

### NATS

- [x] Interrupt the single local NATS service and observe safe platform degradation.
      The initial node-local check failed, was remediated, and the rebuilt-image
      retest passed on all three nodes within 12 seconds.
- [x] Restore it and verify subscriptions, consumers, desired state, and routes
      converge without duplication.
- [x] Record this as dependency interruption only; the single-host test cannot pass
      an HA NATS quorum requirement.

### PostgreSQL

- [x] Stop PostgreSQL and verify readiness becomes not-ready without exhausting
      connections, CPU, or logs.
- [x] Restore PostgreSQL and verify connection-pool recovery.
- [x] Test invalid credentials, maximum connections, slow queries, and migration
      locking behavior.
- [x] Confirm frontend/static routes do not conceal backend database failure.

### Part 3 record - PostgreSQL authentication, pressure, deadlines, and migrations

Status: **PASS on 2026-08-29 after closing runtime deadline, telemetry
amplification, and migration serialization/atomicity gaps.** The canonical local
runner is:

```bash
PGPASSWORD='set-without-shell-history' \
  CARGO_TARGET_DIR=/tmp/openid-connect-wasi-target \
  bash scripts/vm/validate-postgres-failure-modes.sh \
    --state-file .prod-validation-single-host-state.json \
    --application-dir /mnt/d/dev/openid_connect_wasi
```

The runner resolves PostgreSQL and HAProxy only from the selected state and
labels every temporary container with a hash of that state. Cleanup inspects the
exact container identity and label before removal. The runner is disruptive and
local-test-only; never point it at production.

Authentication and exhaustion evidence:

- a disposable PostgreSQL 17 client using an invalid password failed with
  `password authentication failed` in 1.21 seconds. The native `oidc-migrate`
  binary failed with the same bad credential without retrying, while live OIDC
  database readiness remained healthy;
- PostgreSQL reported `max_connections=200` and
  `superuser_reserved_connections=3`. A deterministic 196-client `pgbench`
  workload plus the exporter produced 197 observed sessions, the full
  non-superuser capacity. Additional connections received the expected reserved
  slot error;
- public backend readiness returned HTTP 503 in about 1.2 ms once HAProxy had
  withdrawn the exhausted pool. The frontend remained HTTP 200 in about 2.9 ms,
  every platform node stayed alive, and NATS/eBPF health was unaffected; and
- all 196 held client transactions completed without failure. After release,
  backend readiness recovered to HTTP 200 and `database=ok` in 37.7 ms, the
  exporter returned to one session, and no lingering validation session or alert
  remained.

Slow-query and telemetry evidence:

- the WASI backend deployment URL now applies a two-second connect timeout, a
  five-second statement timeout, a two-second lock timeout, and a 30-second
  idle-in-transaction timeout. These non-secret values are included in the
  content-addressed deployment identity, so a configuration change cannot reuse
  an already-running instance with stale deadlines;
- an exclusive lock on `users` followed by a valid-shape login request forced
  the real application database path. It returned a sanitized
  `server_error` HTTP 500 in 2.029 seconds without exposing database details.
  Readiness concurrently remained HTTP 200 because its independent `SELECT 1`
  was not blocked. A direct `pg_sleep(10)` with a two-second statement timeout
  was canceled as expected;
- the first table-lock experiment exposed monitoring amplification: PostgreSQL
  exporter queries against `pg_stat_user_tables` waited on the application lock
  and accumulated across scrapes. The exporter DSN now limits connect to two
  seconds, statements to two seconds, locks to one second, and idle transactions
  to five seconds; and
- during the corrected 30-second lock retest, six samples over 12 seconds each
  found zero lock-waiting sessions. The table-statistics query count stayed at
  the sampling query itself rather than growing, proving bounded cancellation.
  Exporter failure is allowed to appear as monitoring degradation during a long
  database lock; it must never amplify the database incident.

Migration evidence and implementation:

- `oidc-migrate`, `oidc-wasm-dev`, and `oidc-dev` now acquire the same
  session-level PostgreSQL advisory lock (`734662019`) before creating or reading
  `_migrations`. Acquisition has a ten-second lock timeout, so parallel release
  jobs serialize without waiting forever;
- each migration SQL batch and its `_migrations` insert now execute in one
  explicit transaction. An error rolls back both schema work and the tracking
  row. The embedded migration files contain no independent transaction control,
  so they are compatible with this outer transaction;
- while another session held the advisory lock, the release migrator failed in
  10.06 seconds with the explicit context
  `failed to acquire the migration lock within 10s` and PostgreSQL SQLSTATE
  `55P03`. OIDC remained ready because the advisory lock is migration-specific;
  and
- after release, the migrator completed in 0.08 seconds. `_migrations` contained
  11 rows and 11 distinct filenames, proving no duplicate tracking state. The
  full reusable runner repeated every fault and passed, and the focused Chromium
  login/dashboard suite then passed 6/6 in 4.0 seconds.

Production requirements remain stricter: use a dedicated least-privilege
migration identity, run exactly one migration job before application rollout,
alert on lock timeout and prolonged connection saturation, budget administrative
connections separately, and set pool limits below PostgreSQL's non-superuser
ceiling. Transactional DDL protects these PostgreSQL migrations, but any future
non-transactional operation must have an explicit idempotent recovery design.

### Storage and resources

- [x] Test low disk space and inode exhaustion on disposable images.
- [x] Test read-only storage. Deterministic block-device delay is not supported by
      the current Firecracker testbed and remains a two-host/provider fault gate.
- [x] Constrain CPU and memory and verify backpressure activates before host failure.
- [x] Exercise file-descriptor, connection, and ephemeral-port limits safely.
- [x] Confirm the monitoring pipeline itself does not amplify an exhaustion event.

### Part 4 record - storage and resource pressure

Status: **PASS WITH AN EXPLICIT PERSISTENT-WASI-CLI CONNECTION LIMITATION on
2026-08-29.** The tests used only exact state-recorded node identities and
disposable cloned root filesystems. The production OIDC application remained
available through peer nodes, and the environment remains live for Phase 8.

Storage evidence and fixes:

- an 800-MiB synchronous write reduced node 0 to 739 MiB free, made readiness
  return HTTP 503, and fired `PlatformNodeDiskSpaceExhausted`; a peer and public
  OIDC readiness remained HTTP 200. A shorter repeat returned successfully and
  removed its file;
- creating 115,000 files left 9,419 free inodes while 1,538 MiB remained free.
  The new independent inode threshold made readiness return HTTP 503 and fired
  `PlatformNodeInodesExhausted`, proving byte-only disk monitoring is
  insufficient;
- storage health now reports both available bytes and inodes, with hard and
  warning thresholds exported as `wasm_node_disk_free_mb` and
  `wasm_node_disk_free_inodes`;
- a read-only root kept the Firecracker VMM alive but prevented redb startup and
  withdrew the node. PID 1 originally retried every two seconds; it now uses
  bounded 2/4/8/16/30-second exponential restart backoff. Production immutable
  roots therefore require a distinct writable state volume; and
- long synchronous WASI work must not be its own in-band health check. Epoch
  interruption can prevent guest `Drop`/finally cleanup, so destructive storage
  faults must be cleaned by replacing the disposable VM, not by trusting guest
  cleanup code. The testbed has no deterministic Firecracker block-latency
  injector, so delayed-device semantics are not claimed here.

CPU and memory evidence:

- node 0 was restarted once at one vCPU and 1 GiB. A five-second CPU probe
  completed; a longer probe exhausted its fuel after about 15.6 seconds while
  node admin health stayed responsive, OIDC remained HTTP 200 through peers,
  queue depth stayed bounded, and saturation remained zero;
- holding 400 MiB produced 856/989 MiB effective use (87%), set
  `accepting_requests=false`, activated memory backpressure/eBPF pressure, and
  fired `PlatformNodeMemoryPressure`. The request completed, memory returned to
  458 MiB, readiness recovered, and the alert cleared; and
- memory metrics now expose the effective limit and usage percentage, based on
  the smallest configured, physical, or cgroup ceiling.

FD and connection evidence:

- `max_open_fds` is now applied to Wasmtime's Preview 2 `ResourceTable`; the
  previous core-Wasm table limiter did not cover WASI descriptors or sockets;
- a WASI HTTP canary with a 64-handle ceiling opened until the table was full.
  The offending request returned 502 with the complete
  `resource table has no free keys` cause, `wasm_policy_fd_denied_total`
  increased to 1, the server stayed alive, and the immediately following request
  returned 200;
- the deployment CLI previously discarded `--max-open-fds` unless an allowed
  filesystem path was also present. Policy construction now always transmits the
  explicit ceiling;
- a request-scoped WASI HTTP canary configured for 32 outbound connections and
  a larger ResourceTable opened exactly 32, returned a permission denial for the
  33rd, exposed 32 active connections during the hold, and returned the active
  gauge to zero when the request store ended. The policy limit is far below the
  guest ephemeral-port range, so exhaustion was bounded by policy rather than
  host port availability;
- persistent WASI CLI HTTP servers expose a Wasmtime limitation: the address
  policy hook observes connect but not socket drop, so reservations are released
  only when the instance store is destroyed. For this model the current counter
  is a safe but availability-reducing instance-lifetime budget, not a true
  simultaneous count. Production workloads requiring persistent outbound
  sockets must use request-scoped WASI HTTP, dedicated processes/cgroups, or add
  an authoritative socket-drop host wrapper before acceptance; and
- the node now registers Prometheus' process collector. Every final node exported
  `process_open_fds=144` and `process_max_fds=1024` (about 14%), giving
  `PlatformNodeFileDescriptorsHigh` real input instead of an absent series.

Operational and final gates:

- success logging remained bounded, restart loops use exponential backoff, and
  the pressure tests caused no telemetry target or OIDC availability loss;
- deployment verification now supports `--verify-contains` and the shared agent
  skill requires application-identity evidence. This closed a false-positive
  where HAProxy's default OIDC frontend returned 200 for an unknown canary host;
- the final eBPF-enabled rootfs SHA-256 is
  `ce9f190a21169a321d622aa7ea135ae8912a0a6d0b04ec0c3235564c9a4e5d51`.
  An intermediate direct build omitted `--features ebpf`; the rolling gate caught
  `feature_not_compiled` before the last node, and the corrected image was then
  rolled one node at a time with `kernel monitoring active` verified on each;
- after the rollout all three `platform-node` targets reported `up=1`, OIDC
  readiness returned `database=ok`, no Prometheus alert was firing, and the
  focused Chromium login/dashboard suite passed 6/6 in 4.9 seconds; and
- all Part 4 resource canaries and routes were explicitly undeployed. The NATS,
  PostgreSQL, platform, HAProxy, and observability environment was intentionally
  left running for the next authorized phase. A final inventory also found and
  removed the exact zero-instance superseded backend
  `oidc/openid-connect-wasi:vd5fc09739b3d`; the inventory now contains only the
  active frontend and backend, and the `validation` namespace is empty.

## Phase 8: capacity, soak, upgrade, and rollback

- [x] Define representative request mixes, data size, sessions, concurrency, and
      think time.
- [x] Run baseline, ramp, spike, and a bounded local soak test.
- [x] Repeat the target load with one platform node unavailable.
- [x] Monitor p50/p95/p99 latency, error rate, CPU, memory, connections, traps,
      database pools, NATS lag, telemetry queues, and disk I/O.
- [x] Identify the first saturated resource and establish a conservative capacity
      envelope for this host only.
- [x] Deploy a new immutable application version identity during load.
- [x] Verify migration compatibility with old and new version identities concurrently.
- [x] Stop the rollout on a synthetic failure and restore the last-known-good
      artifact and configuration.
- [x] Confirm rollback does not require deletion of current state.

Local results demonstrate behavior and provide a lower-bound capacity measurement;
they are not production capacity figures.

### Phase 8 Part 1 result: configured capacity and short soak

The reusable runner is `scripts/vm/validate-oidc-capacity.sh`. It extends the
bounded Rust HTTP benchmark with pacing, POST bodies loaded from a protected file,
explicit accepted statuses, and response-status histograms. The production-like
request contract was fixed before the run:

- 50% frontend document requests;
- 25% OIDC discovery requests;
- 20% database-backed readiness requests;
- 5% authenticated login requests using the disposable seeded account; and
- persistent clients, bounded concurrency, and evenly paced requests rather than
  an unpaced connection storm.

The acceptance contract was zero transport errors and zero HTTP 5xx responses;
HTTP 429 was recorded separately as an intentional admission-control boundary.
The local target was p99 below 250 ms for frontend/discovery/readiness and below
2 seconds for password-backed login, no HAProxy queue, no monitoring loss or
parser error, bounded memory, and healthy PostgreSQL/NATS telemetry.

Observed traffic results:

| Stage | Result | Interpretation |
| --- | --- | --- |
| Baseline, 5 requests/s for 20 seconds | 100/100 HTTP 200 | Accepted |
| Ramp, 15 requests/s for 20 seconds | 74 HTTP 200, 226 HTTP 429 | Front-door admission boundary crossed |
| Ramp, 30 requests/s for 20 seconds | 600/600 HTTP 429 | Fully admission-limited |
| Spike, 60 requests/s for 20 seconds | 1,200/1,200 HTTP 429 | Fully admission-limited; no downstream saturation |
| Deliberate over-limit hold, 15 requests/s for 120 seconds | 1,800/1,800 HTTP 429 | Proves limiter stability, not application soak capacity |
| Accepted short soak, 5 requests/s for 120 seconds | 600/600 HTTP 200 | PASS |
| N−1 representative mix, 5 requests/s for 30 seconds | 151/151 HTTP 200 | PASS with node 0 drained and absent |
| N−1 readiness probe, 15 requests/s for 30 seconds | 98 HTTP 200, 352 HTTP 429 | Current policy has no 15 requests/s N−1 allowance |

The first limit is configuration, not CPU: the generated HAProxy frontend tracks
the client IP over ten seconds and denies when `sc_http_req_rate(0) gt 100`.
Consequently this topology deliberately caps one client IP around 10 requests/s
even though all three platform nodes have a 10 requests/s per-IP default. The
safe declared rehearsal envelope is 5 requests/s per client IP, retaining 50%
front-door headroom and passing with one node absent. Do not publish this as a
VPS capacity number. Production sizing must set an intentional tenant/client
identity model and admission budget, then repeat the test at that policy; raising
the HAProxy threshold merely to obtain a larger benchmark number is not evidence
of safe capacity.

During the accepted soak, frontend/discovery/readiness/login p99 was respectively
5.2/54.7/57.1/166.4 ms. Maximum node memory was 27%, maximum platform process RSS
was 578,183,168 bytes, maximum one-minute process CPU rate was 0.091 core,
PostgreSQL used at most two backend sessions, HAProxy backend queue stayed zero,
NATS pending bytes and slow consumers stayed zero, and eBPF dispatcher saturation
and parse-error deltas stayed zero. Dispatcher depth peaked at 15 and remained
well below its bound.

Operational observations:

- load credentials are written to a mode-600 temporary file and are not exposed
  in benchmark process arguments;
- the runner now uses non-interactive `sudo -n` and tells the operator to run
  `sudo -v` first, avoiding an invisible password wait in automation;
- HTTP 429 must not be counted as successful application work merely because the
  limiter behaved correctly; the status histogram is the capacity truth;
- a short two-minute soak is adequate for this local gate but production should
  add a multi-hour endurance run at the declared target; and
- evidence is preserved under `/tmp/phase8-part1-oidc-capacity-20260829`,
  `/tmp/phase8-part1-oidc-soak-20260829`, and
  `/tmp/phase8-part1-oidc-nminus1-20260829` until WSL restarts or `/tmp` is cleaned.

After N−1 testing, node 0 was rebuilt as PID `283570`. The final state has all
three nodes healthy, OIDC readiness reports `database=ok`, all ten Prometheus
targets are up, and the alert set is empty. The environment remains live for the
separately authorized upgrade/rollback part.

### Phase 8 Part 2 result: upgrade, migration compatibility, and rollback

The state-scoped runner is `scripts/vm/validate-oidc-upgrade-rollback.sh`. It
records the existing route as the last-known-good target, refuses to proceed
unless database readiness and guest clock skew pass on every node, keeps a paced
load running during route changes, and has an exit trap that restores the exact
original route before removing test candidates.

The immutable release record for this run was:

```text
last known good: oidc/openid-connect-wasi:vdc1902621308
candidate:       oidc/openid-connect-wasi:vdc1902621308-phase8-candidate
Wasm SHA-256:    c9fdafa74897137a826d2a1f1903557b05ed02772701106e738e6e6056c39628
migrations:      11 total / 11 distinct before and after
```

The candidate deliberately uses the same binary digest under a new immutable
release identity. This proves control-plane convergence, route cutover, runtime
configuration, additive-schema compatibility, canary abort, and rollback. It does
not prove compatibility of code that does not exist in this checkout. The OIDC
working tree contains extensive pre-existing uncommitted changes, and the live
artifact was built from that state; fabricating another source change would not
be credible release evidence. The production release must repeat this procedure
with different signed old/new digests and their actual migration set.

Migration compatibility was exercised by taking the normal migration advisory
lock and transactionally adding a disposable `phase8_upgrade_probe` table plus a
unique `V9000__phase8_additive_probe.sql` tracking record. Both the old and
candidate identities continued to serve database-backed readiness on every node,
and the normal migrator remained idempotent while both were live. Rollback removed
only the exact probe table/tracking row created by this test; the pre-existing
11/11 migration set and application data were unchanged.

The compatible rollout sequence passed as follows:

1. The candidate was deployed on a distinct canary host and returned
   `database=ok` directly through every node.
2. The last-known-good identity received a temporary compatibility alias, so old
   and candidate instances were independently reachable against the expanded
   schema.
3. `oidc-backend.internal` was switched to the candidate while 450 readiness
   requests ran at 5 requests/s. All 450 returned HTTP 200; p50/p95/p99/max were
   34.3/47.1/68.5/76.9 ms.
4. A second candidate was successfully authorized and deployed with a malformed
   database URL. Its readiness returned HTTP 500, so the rollout stopped at the
   canary and it never received the public route.
5. The public route was restored to `vdc1902621308`, the critical transaction
   passed, and only then were candidate routes and desired-state entries removed.

#### Clock-skew incident found during the final browser gate

The first post-rollback Playwright run exposed intermittent expired sessions even
though login and readiness returned HTTP 200. Its trace showed an access token
with `scope=admin`, but the issuing node's JWT `exp` was roughly 2.3 hours behind
the browser. Nodes 1 and 2 had retained pre-suspend time when the laptop/WSL host
resumed; recently restarted node 0 had current time. HAProxy could therefore send
login to a stale-clock node, producing a token that the SPA immediately rejected.

This is a production-critical dependency: authentication, TLS, logs, leases, and
distributed coordination cannot rely on readiness alone when clocks diverge. The
node image now includes Chrony, performs a bounded initial synchronization before
starting `wasm-node`, refuses startup if synchronization fails, and keeps the
daemon running. The disposable image uses a fixed public Cloudflare NTP address;
production must configure multiple authenticated or operator-controlled internal
time sources and alert on offset/stratum/source loss. Node image schema increased
from 5 to 6 so cached images without this boot contract are rejected.

All three nodes were rolled to schema 6. Before the final upgrade rehearsal their
HTTP dates were within one second of the load generator; afterward they matched
within the recorded one-second resolution. The focused login/dashboard Chromium
suite then passed 6/6 in 4.1 seconds.

Two related operator observations were preserved:

- generic `/healthz` becoming HTTP 200 only proves node liveness. Immediately
  after a restart, the application route returned HTTP 500 for several seconds
  while artifacts and instances reconstructed. Rolling automation must wait for
  application-aware readiness on the replacement node before continuing; and
- a previously drained node can remain alive but `accepting_requests=false` with
  zero applications. The Phase 8 preflight caught this even though the generic VM
  status command reported HTTP 200. A full local-state rebuild plus recorded
  restart restored desired state. Production orchestration must distinguish
  liveness, node readiness, and application readiness and must not count a drained
  node as recovered capacity.

Final evidence is in `/tmp/phase8-part2-oidc-upgrade-20260829-final`. The rootfs
digest is `347430e89c35af05d55a634542842c5e272201fda887991ec7973da3c133731b`.
The final route inventory has exactly the original frontend and backend; no Phase
8 application metric series remains. Prometheus reports 10/10 targets, no active
alert, and `sum(wasm_ebpf_active)=3`. The environment remains live and was not
destroyed.

## Phase 9: backup and restore

Reusable isolated restore check:

```bash
PGPASSWORD='set-without-shell-history' \
  bash scripts/vm/validate-postgres-backup.sh \
  --state-file .prod-validation-single-host-state.json
```

Full application-level recovery rehearsal:

```bash
PGPASSWORD='set-without-shell-history' \
  CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target \
  bash scripts/vm/validate-oidc-disaster-recovery.sh \
  --state-file .prod-validation-single-host-state.json \
  --app-dir /mnt/d/dev/openid_connect_wasi \
  --report-dir /tmp/phase9-oidc-recovery-$(date -u +%Y%m%dT%H%M%SZ)
```

- [x] Back up PostgreSQL using the intended logical or physical procedure.
- [x] Record artifact/control state required to rebuild platform nodes.
- [x] Restore the database into a separate disposable PostgreSQL instance.
- [x] Run schema/version checks, record counts or checksums, readiness, and a full
      OIDC journey against the restored instance.
- [x] Measure recovery point and recovery time.
- [x] Verify backup encryption, access controls, retention metadata, and restore
      documentation without using production keys.
- [x] Confirm a platform node can be destroyed and rebuilt from immutable artifacts
      and declared configuration without copying another live node filesystem.

### Phase 9 result: application-level recovery rehearsal

The final reusable runner is `scripts/vm/validate-oidc-disaster-recovery.sh`.
It refuses an existing report directory, derives all service/node addresses from
the selected topology state, uses a unique immutable recovery identity per report,
and installs an exit trap before changing any route. The trap restores the exact
last-known-good backend, removes only the recovery host and application, drops
only the source marker table created by the run, and removes only the labeled
restore container and private plaintext workspace.

The final evidence is `/tmp/phase9-oidc-recovery-20260829-final4`. The database
backup was PostgreSQL custom format and the encrypted artifact is
`oidc.dump.gpg`, SHA-256
`e20b30f120e33183b3f6a6cc1670537b4133b8b77b18149f1c3a0b43bf904229`.
The report directory is mode 700 and every retained evidence file is mode 600.
The disposable encryption passphrase was used to decrypt and verify the exact
plaintext SHA-256, then destroyed with the plaintext workspace. This validates
the local encryption procedure without embedding a key in the evidence, but it
does not validate production KMS/HSM recovery, key rotation, separation of
duties, off-host replication, object lock, or retention enforcement.

The snapshot boundary uses two deterministic recovery markers. The marker
committed immediately before `pg_dump` exists after restore, while a transaction
committed after the dump does not. All 26 public tables in that snapshot matched
by row count and a deterministic hash of every logical row; PostgreSQL was 17.11
on both sides and migration tracking was 11 total/11 distinct. The restore
backend returned `database=ok` through every platform-node proxy. After temporary
public cutover, the focused Chromium login/dashboard suite passed 6/6 in 4.4
seconds, including invalid and successful login behavior. The original public
backend was restored before the recovery deployment and container were removed.

Measured local timings from the final run were:

| Measurement | Result | Interpretation |
|---|---:|---|
| Observed snapshot boundary | 0.002 s | Host-observed marker commit to dump start; this is not the production scheduled-backup interval |
| Logical dump | 1.221 s | Small local OIDC database only |
| Database restore RTO | 6.338 s | Recovery start through successful `pg_restore` and database verification |
| Application readiness RTO | 43.470 s | Recovery start through restored-backend readiness and public cutover |
| Full journey RTO | 52.053 s | Recovery start through all six Playwright checks |

These values demonstrate that the mechanism can recover on this host. Production
RPO is instead bounded by backup frequency, replication/archive lag, and the last
independently verified off-host recovery point. Production RTO must add incident
detection, operator or automation reaction time, artifact/secret retrieval,
network/DNS changes, larger data volume, cache warm-up, and approval gates.

#### PostgreSQL clock defect and image remediation

The first report incorrectly appeared to have an approximately 12,329-second RPO.
The recovery markers revealed that the running PostgreSQL schema-3 microVM had
retained time from before a laptop/WSL suspend, just as the platform-node images
had earlier. The final run measured 12,398.867 seconds of source database clock
skew. Database time is therefore recorded separately and is never used as the
runner's RPO clock. This matters for audit order, retention, token/session expiry,
scheduled jobs, point-in-time recovery, and timestamp-based conflict handling.

The PostgreSQL rootfs contract is now schema 4. It installs Chrony, requires a
bounded initial synchronization before PostgreSQL starts listening, and keeps
continuous synchronization running. The disposable local image uses a fixed
public Cloudflare NTP source; production requires multiple authenticated or
operator-controlled time sources plus offset, stratum, reachability, and source-
loss alerts. An alternate schema-4 image built successfully at
`/tmp/phase9-postgres-schema4/postgres-rootfs.ext4`, contains `chronyd`, passes
read-only `e2fsck`, and has SHA-256
`9249f9507ce2abe3cc787e557e99b2e0099c0aa7ca0344382ededf1233272ad4`.

The live schema-3 database disk was deliberately not overwritten. The builder
now refuses to replace the canonical PostgreSQL image whenever a PostgreSQL PID
from a repository state file still exists, and `provision-postgres-service.sh`
rejects pre-schema-4 images. The next fresh environment must use schema 4; this
running environment remains a valid data-recovery source but is explicitly not
evidence of continuous database time synchronization.

One repeat run reused the exact recovery version and timed out with HTTP 503.
The cleanup trap restored the last-known-good route and removed the container.
Recovery versions are now derived from the unique report path, and two subsequent
fresh-identity rehearsals passed. Operational automation must treat recovery and
rollback identities as immutable, single-use release records rather than rapidly
recycling a previously undeployed identity.

## Phase 10: result and teardown

The single-host plan passes only when:

- [ ] All required build, application, routing, observability, failure, upgrade,
      rollback, and restore checks pass.
- [ ] All limitations are explicitly recorded as `NOT VALIDATED` or approved
      exceptions.
- [ ] No unresolved P0/P1 defect applies to the tested production design.
- [ ] The complete evidence package is reviewable by someone other than the test
      operator.

If interactive testing is complete and the user explicitly authorizes teardown:

```bash
bash scripts/vm/destroy-testbed.sh --state-file "$STATE_FILE"
```

- [x] Verify only PIDs, TAP devices, bridges, HAProxy configuration, and service
      state recorded for this testbed were removed.
- [x] Verify the state file and companion state were removed.
- [ ] Preserve reports, metrics snapshots, relevant redacted logs/traces, checksums,
      and the final decision record.

## Handoff to multi-host validation

Do not repeat every functional test manually on two hosts. Promote the same release
artifacts, configuration schema, application artifacts, synthetic journeys, load
profiles, dashboards, and alert rules into the two-host plan. The multi-host plan
focuses on real host loss, routed cross-host networking, independent storage, and
N-1 capacity.

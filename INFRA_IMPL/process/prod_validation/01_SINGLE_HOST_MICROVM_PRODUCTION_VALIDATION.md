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

## Execution record: 2026-08-23 / single-host run in progress

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

## Phase 0: safety and prerequisites

- [x] Run all commands inside Linux or WSL2 from the repository root.
- [ ] Confirm the target is a disposable local test environment.
- [ ] Confirm no production credentials, certificates, customer data, or DNS
      zones are present.
- [x] Confirm KVM is accessible:

  ```bash
  test -r /dev/kvm && test -w /dev/kvm
  ```

- [ ] Confirm sufficient free CPU, memory, and disk for three 2 GiB platform
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
- [ ] Define the SLOs and concrete pass thresholds before generating traffic.

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

- [ ] Preserve unrelated worktree changes and record `git status --short`.
- [ ] Confirm `rust-toolchain.toml` and `Cargo.lock` are committed.
- [ ] Set build output on the Linux filesystem when the checkout is under `/mnt`:

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
- [ ] Review effective production-like configuration for unique node IDs,
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
- [ ] Verify HAProxy exposes no node administration endpoint publicly.
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
- [ ] Callback, token/session handling, authenticated API access, refresh where
      supported, and logout all work through the public origin.
- [ ] Invalid credentials, invalid redirect URIs, expired state/nonce, and expired
      sessions fail safely.
- [ ] No password, JWT, cookie, authorization header, database URL, or client
      secret appears in logs or traces.

Retain the environment for interactive browser testing. Do not destroy it until
the user explicitly finishes testing.

## Phase 4: metrics and alerting

The code currently exposes platform metrics at `/metrics`. Reconcile any
configuration or documentation that still refers to `/status/metrics` before
accepting this phase.

- [ ] Deploy or connect a disposable Prometheus and Alertmanager.
- [ ] Scrape every platform node separately over the intended private path.
- [ ] Configure the actual authentication and TLS behavior of `/metrics`.
- [ ] Add host, HAProxy, NATS, PostgreSQL, and OpenTelemetry Collector metrics.
- [ ] Assign stable `environment`, `cluster`, `node`, `job`, and `instance` labels.
- [ ] Run:

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

Run this phase only on a kernel with the required BTF/eBPF support. The normal
release currently does not enable the node's `ebpf` Cargo feature; build a clearly
identified test artifact with that feature before claiming kernel monitoring is
active.

- [ ] Record guest kernel version, configuration, BTF availability, capabilities,
      eBPF object checksum, and release feature set.
- [ ] Confirm every eBPF object is packaged from a production-safe path.
- [ ] Start in userspace fallback mode and record baseline metrics and overhead.
- [ ] Enable one probe group at a time.
- [ ] Confirm `wasm_ebpf_active` reflects the actual mode.
- [ ] Confirm events are scoped to the intended platform cgroup/PID namespace and
      do not expose unrelated processes.
- [ ] Generate file, TCP, memory-pressure, syscall, and block-I/O events and verify
      the corresponding metrics.
- [ ] Measure ring-buffer/event loss and dispatcher backpressure.
- [ ] Deny eBPF loading and confirm the node continues safely in fallback mode.
- [ ] Restart the node and unload/reload probes without leaving pinned maps or
      programs.
- [ ] Compare request latency, CPU, and memory against the fallback baseline.

Do not pass this phase while probes remain system-wide, object paths are tied to
a development checkout, unsafe event parsing is unvalidated, or the primary node
retains broad capabilities unnecessarily.

## Phase 7: controlled failure tests

Run one fault at a time first, then selected combinations. Always record the exact
target from the state file; never kill processes or remove networking by broad
pattern matching.

### Platform node

- [ ] Gracefully stop one platform node.
- [ ] Abruptly terminate one platform-node Firecracker process.
- [ ] Pause or isolate one node long enough to trigger health removal.
- [ ] Verify HAProxy stops routing to the unhealthy application pool.
- [ ] Verify remaining capacity stays within the declared SLO.
- [ ] Restore or rebuild the node and verify membership and deployment convergence.

### Network

- [ ] Add latency, jitter, packet loss, and bandwidth restriction to one node.
- [ ] Partition one platform node from NATS.
- [ ] Partition one platform node from PostgreSQL.
- [ ] Verify timeouts are bounded and recovery does not cause a retry storm.
- [ ] Verify stale routes or application instances are not advertised indefinitely.

### NATS

- [ ] Interrupt the single local NATS service and observe safe platform degradation.
- [ ] Restore it and verify subscriptions, consumers, desired state, and routes
      converge without duplication.
- [ ] Record this as dependency interruption only; the single-host test cannot pass
      an HA NATS quorum requirement.

### PostgreSQL

- [ ] Stop PostgreSQL and verify readiness becomes not-ready without exhausting
      connections, CPU, or logs.
- [ ] Restore PostgreSQL and verify connection-pool recovery.
- [ ] Test invalid credentials, maximum connections, slow queries, and migration
      locking behavior.
- [ ] Confirm frontend/static routes do not conceal backend database failure.

### Storage and resources

- [ ] Test low disk space and inode exhaustion on disposable images.
- [ ] Test read-only or delayed storage where supported.
- [ ] Constrain CPU and memory and verify backpressure activates before host failure.
- [ ] Exercise file-descriptor, connection, and ephemeral-port limits safely.
- [ ] Confirm the monitoring pipeline itself does not amplify an exhaustion event.

## Phase 8: capacity, soak, upgrade, and rollback

- [ ] Define representative request mixes, data size, sessions, concurrency, and
      think time.
- [ ] Run baseline, ramp, spike, and sustained soak tests.
- [ ] Repeat the target load with one platform node unavailable.
- [ ] Monitor p50/p95/p99 latency, error rate, CPU, memory, connections, traps,
      database pools, NATS lag, telemetry queues, and disk I/O.
- [ ] Identify the first saturated resource and establish a conservative capacity
      envelope for this host only.
- [ ] Deploy a new application version during load.
- [ ] Verify migration compatibility with old and new versions concurrently.
- [ ] Stop the rollout on a synthetic failure and restore the last-known-good
      artifact and configuration.
- [ ] Confirm rollback does not require deletion of current state.

Local results demonstrate behavior and provide a lower-bound capacity measurement;
they are not production capacity figures.

## Phase 9: backup and restore

- [ ] Back up PostgreSQL using the intended logical or physical procedure.
- [ ] Record artifact/control state required to rebuild platform nodes.
- [ ] Restore the database into a separate disposable PostgreSQL instance.
- [ ] Run schema/version checks, record counts or checksums, readiness, and a full
      OIDC journey against the restored instance.
- [ ] Measure recovery point and recovery time.
- [ ] Verify backup encryption, access controls, retention metadata, and restore
      documentation without using production keys.
- [ ] Confirm a platform node can be destroyed and rebuilt from immutable artifacts
      and declared configuration without copying another live node filesystem.

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

- [ ] Verify only PIDs, TAP devices, bridges, HAProxy configuration, and service
      state recorded for this testbed were removed.
- [ ] Verify the state file and companion state were removed.
- [ ] Preserve reports, metrics snapshots, relevant redacted logs/traces, checksums,
      and the final decision record.

## Handoff to multi-host validation

Do not repeat every functional test manually on two hosts. Promote the same release
artifacts, configuration schema, application artifacts, synthetic journeys, load
profiles, dashboards, and alert rules into the two-host plan. The multi-host plan
focuses on real host loss, routed cross-host networking, independent storage, and
N-1 capacity.

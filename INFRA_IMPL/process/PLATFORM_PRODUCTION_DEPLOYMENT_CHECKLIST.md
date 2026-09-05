# Platform production deployment checklist

## Purpose and boundary

Use this runbook to decide whether the Wasm Cloud Platform itself can be deployed
into production and to control that rollout. This is separate from onboarding an
application; applications must also pass the
[application deployment readiness checklist](./APPLICATION_DEPLOYMENT_READINESS_CHECKLIST.md).

The Firecracker automation in `scripts/vm/` is a local Linux/WSL2 test harness. It
does not provision production hosts, TLS, highly available NATS, external secrets,
monitoring, backups, or multi-zone networking. Never point the local provision,
deploy, or destroy scripts at production resources.

This checklist is an operator runbook, not a statement that every listed capability
is automated today. Items marked as requirements need deployment evidence from the
chosen infrastructure, even when the platform code has unit or integration coverage.

The boundary is the Wasm Cloud Platform. NATS is a required platform dependency,
but PostgreSQL is only a dependency of applications such as the OIDC rehearsal.
Vault Transit and AWS KMS HMAC are supported external seal-root integrations;
the platform does not deploy Vault, KMS, or an HSM. Likewise, Prometheus, log,
and trace backends consume platform telemetry but remain external services.
Platform release gates prove client interoperability, fail-safe behavior, and
recovery. Availability, backup, PKI, and lifecycle qualification of the chosen
external products belong to the deployment operator's infrastructure gate.

## Release decision rules

Classify every required gate as `PASS`, `FAIL`, or `EXCEPTION`. Each exception needs
a risk statement, compensating control, owner, approver, review date, and expiry.

Do not open production traffic when any of these conditions is true:

- a P0/P1 finding applicable to the production threat model remains unresolved;
- NATS cannot retain quorum after one planned failure;
- node identity, artifact endpoints, or advertised addresses are not unique and routable;
- administrative or artifact endpoints are publicly reachable without enforced TLS
  and authorization;
- production keys or credentials use placeholder, generated-ephemeral, or local-test values;
- a node cannot be rebuilt from immutable artifacts and declared configuration;
- state backup, restore, or disaster-recovery procedures have not been exercised;
- one-node loss exceeds available capacity or violates the declared availability target;
- dashboards, alerts, and audit records do not receive live data;
- platform rollback has no preserved last-known-good binary/configuration; or
- no named incident commander can stop, roll back, or isolate the deployment.

## 1. Define the production architecture

Record these decisions before creating hosts:

| Area | Required decision |
|---|---|
| Environment | Region(s), availability zones/failure domains, account/project, and environment name |
| Platform nodes | Count, placement, CPU, memory, local disk, expected applications, and N+1 headroom |
| Host substrate | Bare metal, VM, or another supported Linux environment; kernel and KVM/eBPF requirements |
| NATS | Cluster size, JetStream replicas, storage class, retention, authentication, TLS, backup, and recovery |
| North-south traffic | DNS, external load balancer, TLS ownership, WAF/rate limits, health checks, and drain behavior |
| East-west traffic | Routable node addresses, firewall policy, artifact transfer, NATS, DNS, and dependency paths |
| Identity | Node identity, operator identity, workload namespaces, certificates, and authorization model |
| Secrets | Vault/KMS/secret manager, bootstrap identity, rotation, revocation, and break-glass process |
| State | redb placement, artifact/cache volumes, billing exports, backup policy, and rebuild source of truth |
| Observability | Metrics, logs, traces, audit events, dashboards, alerts, retention, and ownership |
| Availability | SLO, RTO, RPO, maintenance policy, disruption budget, and disaster-recovery design |
| Delivery | Release promotion, signature/provenance validation, configuration promotion, rollout, and rollback |

- [ ] Production begins with at least three platform nodes when node-level high
      availability is required.
- [ ] Nodes span independent failure domains where the infrastructure supports them.
- [ ] Capacity remains sufficient after the largest expected single failure and during rollout.
- [ ] Multi-region operation is not assumed unless latency, partition, consistency,
      route convergence, NATS topology, and failover have been explicitly tested.
- [ ] The supported production operating system and CPU architecture are documented.
- [ ] Windows is not treated as a production node target; the current production baseline is Linux.

## 2. Production automation and reproducibility

- [ ] Infrastructure is declared in reviewed IaC or equivalent configuration management.
- [ ] Host creation, firewalling, DNS, load balancers, volumes, NATS, secret policies,
      observability agents, and node installation are reproducible without manual shell history.
- [ ] Production does not use the mutable local microVM rootfs images or test credentials.
- [ ] Node OS images are immutable or converged from a pinned, checksummed baseline.
- [ ] Kernel, Firecracker if used, `wasm-node`, configuration schema, and supporting
      binaries have explicit versions and provenance.
- [ ] Every node receives a unique node ID, host identity, address, local state path,
      and writable filesystem. Writable root filesystems are never shared between nodes.
- [ ] Bootstrap can be repeated without duplicating streams, routes, consumers, or state.
- [ ] A fully destroyed node can be replaced from automation and converge from the
      control-plane source of truth without copying another live node's mutable filesystem.
- [ ] Drift detection identifies manual changes to binaries, config, firewall rules,
      units, certificates, and kernel settings.
- [ ] Staging uses the same automation modules and meaningful production topology,
      differing through reviewed inputs rather than an unrelated installation process.

The repository currently contains a hardened systemd unit for `wasm-deploy-ingress`,
but no canonical checked-in production installer/unit for the main `wasm-node` binary.
Treat creation and validation of that packaging/IaC path as a production gate rather
than copying an illustrative unit from an implementation document.

## 3. Host and operating-system gates

The platform does not distribute a production kernel or require Firecracker.
The operator owns the VPS/host image, kernel, firmware, microcode, patching, and
virtualization policy. The repository's
[`VM_TESTBED_KERNEL_VALIDATION.md`](VM_TESTBED_KERNEL_VALIDATION.md) applies only
to the disposable Firecracker rehearsal environment.

- [ ] The exact production host image satisfies the platform's architecture,
      Linux, filesystem, networking, DNS, and clock requirements.
- [ ] cgroup v2, BTF, tracefs/perf facilities, and required capabilities are
      verified when the corresponding resource-control or eBPF features are
      enabled. Disabled optional features have an explicit operating policy.
- [ ] The VPS/host provider or operating-system owner supplies the required
      kernel, firmware/microcode, vulnerability handling, and security updates.
      This is host admission evidence, not a platform release artifact.
- [ ] KVM/Firecracker checks are required only for deployments that deliberately
      run the platform in Firecracker; native VPS deployments do not require KVM.
- [ ] The node runs as a dedicated unprivileged service account.
- [ ] Files and directories have explicit ownership and modes for configuration,
      TLS private keys, sealing keys, NATS credentials, redb, caches, artifacts, logs,
      billing exports, and upgrade staging.
- [ ] systemd or the selected supervisor defines restart policy, startup ordering,
      graceful stop timeout, file-descriptor limits, process limits, memory/CPU policy,
      and a bounded restart rate.
- [ ] Service hardening is tested: `NoNewPrivileges`, read-only system paths, private
      temporary directories, restricted devices, syscall/capability bounds, and writable-path allowlists.
- [ ] Required eBPF capabilities are granted narrowly. Enabling eBPF does not result
      in an unnecessarily privileged node process.
- [ ] Time synchronization, entropy availability, DNS resolution, certificate trust,
      log rotation, disk trim/monitoring, and security patching are operational.
- [ ] Every host, platform guest, and timestamp-sensitive application dependency
      uses at least three independent operator-approved time sources (NTS or an
      equivalently authenticated private service where available). Offset,
      stratum/source loss, synchronization state, and missing telemetry alert
      before the application clock exceeds its documented five-second maximum.
- [ ] Host suspend/resume or a controlled clock fault is rehearsed. Services either
      recover within the clock bound before accepting traffic or fail readiness;
      backup markers, token/session validity, retention, and audit ordering remain
      correct after recovery.
- [ ] Core dumps and crash artifacts follow the security policy and cannot leak secrets.
- [ ] OS and platform upgrades are rehearsed before production and respect the node disruption budget.

## 4. Network and port exposure gates

Document every listener, caller, network boundary, TLS mode, and firewall rule. A
typical deployment may include:

| Endpoint | Typical purpose | Production exposure |
|---|---|---|
| `80/443` or configured proxy ports | Application ingress | Only through the approved load-balancer/security boundary |
| `9090` | Node admin, health, and metrics | Private management network or loopback behind a trusted proxy |
| `9091` | Artifact transfer | Private node network; authenticated and encrypted when remote |
| `10000-19999` by default | Local Wasm instance listeners | Loopback only; never externally routable |
| `4222` | NATS client traffic | Private network with authentication and TLS |
| NATS route/monitoring ports | NATS clustering and operations | Restricted to cluster/operations identities |
| Database and dependency ports | Application egress | Explicit least-privilege destinations only |
| OTLP/DNS webhook/secret services | Platform integrations | Explicit authenticated egress only |

- [ ] Public security groups/firewalls cannot reach admin, artifact, NATS, redb, or
      Wasm instance ports directly.
- [ ] `runtime.instance_bind_address` remains loopback unless a reviewed architecture
      provides equivalent isolation.
- [ ] Every multi-node installation sets a routable, stable advertised artifact host/URL.
- [ ] Artifact addresses never advertise `127.0.0.1` to peer nodes.
- [ ] Trusted-proxy CIDRs are exact; forwarded client identity is ignored from all other sources.
- [ ] Network MTU, DNS TTL, IPv4/IPv6 behavior, connection tracking, ephemeral-port
      capacity, and idle timeouts are tested under expected load.
- [ ] Node-to-node, NATS, database, secret-manager, and observability paths have
      explicit timeout, retry, and certificate-validation behavior.
- [ ] Firewall-denied and dependency-partition scenarios produce bounded failure and recovery.

## 5. NATS and distributed control-plane gates

NATS/JetStream is a critical production dependency, not a disposable message bus.

- [ ] Use an odd-sized NATS cluster with enough replicas to meet the chosen failure tolerance.
- [ ] JetStream storage is durable, sized, monitored, and placed across failure domains.
- [ ] Stream subjects, retention, maximum age/bytes/messages, replicas, duplicate
      window, consumer ACK policy, and redelivery/backoff are explicitly configured.
- [ ] NATS authentication, account/subject authorization, TLS, certificate rotation,
      and administrative access are enforced.
- [ ] Platform nodes receive only the subjects required by their role and namespace model.
- [ ] One business event is processed once per intended node, and a transient handler
      failure is redelivered instead of acknowledged and lost.
- [ ] Poison messages are bounded, observable, and do not block unrelated events.
- [ ] Fresh-node bootstrap has a single accepted snapshot, is idempotent, and restores
      routes, gateway policy, configuration, and artifact metadata.
- [ ] Late-joining nodes converge after missed deployments and route changes.
- [ ] NATS node loss, leader change, network partition, storage pressure, credential
      rotation, and complete restart have been rehearsed.
- [ ] JetStream backup/restore or stream reconstruction meets the declared RPO/RTO.
- [ ] Alerts cover quorum, replica lag, consumer lag, redeliveries, storage, and publish failures.

The local testbed creates one NATS microVM. It is useful for protocol validation but
provides no evidence for production NATS high availability.

## 6. Identity, authentication, and secrets gates

Apply the detailed [production secret lifecycle](./PRODUCTION_SECRET_LIFECYCLE.md)
and attach its external-manager and redaction evidence to the change record.

- [ ] Replace every `CHANGE-ME` value in the production template through external
      secret delivery; fail deployment if placeholders remain.
- [ ] Use structured read/write admin authentication. Legacy single-token mode is
      not accepted as production guidance.
- [ ] `auth.require_tls = true` is validated at startup with readable, correct TLS material.
- [ ] Read and write identities are separate, least-privilege, rotated, revocable,
      audited, and not shared by humans and automation.
- [ ] Admin and artifact access is identity-aware at the network and application layers.
- [ ] Gateway OIDC has an exact HTTPS issuer, expected audience, accepted algorithms,
      claim mapping, token lifetime/skew policy, and fail-closed behaviour.
- [ ] If JWKS uses a private split-horizon endpoint, it has independent DNS/TLS/egress
      controls while token `iss` validation remains bound to the public issuer.
- [ ] JWKS refresh, overlapping old/new signing keys, provider outage, malformed,
      expired, wrong-issuer, wrong-audience, missing-scope, and wrong-role paths are tested.
- [ ] Public, authenticated, role-protected, and scope-protected routes are verified
      through every node and the production front door using a non-production test identity.
- [ ] Set `node.environment = "production"`; prove admission rejects local defaults.
- [ ] The node runtime sealing root uses pinned Vault Transit HMAC or AWS KMS HMAC
      with a non-exportable key. Generated, file, command, KV-exported, environment,
      and passphrase sources are not admitted for production nodes.
- [ ] Private Vault PKI is supplied through `runtime.key_vault_ca_cert`; SAN
      validation passes and no trust-all or plaintext fallback exists.
- [ ] Vault Transit uses `type=hmac key_size=32` without `derived=true`; the
      stable unique node context is HMAC input/domain separation.
- [ ] Deploy ingress runs in its production mode and receives its derived envelope
      key from a secret-agent read-only tmpfs projection with mode 0600 or stricter.
- [ ] Bootstrap credentials are short-lived and cannot be reused as steady-state credentials.
- [ ] Key/certificate/token rotation is tested without losing encrypted data or cluster availability.
- [ ] Secret deletion reaches every authoritative registry node, including a stale
      node after reconnect, and rotated/revoked application instances are evicted.
- [ ] External seal-root rotation rewraps both the persisted KEK and node transport
      key; every node restarts successfully after the previous key is removed.
- [ ] Break-glass access is time-bounded, separately audited, and exercised before an incident.
- [ ] Secret values never appear in CLI arguments, process listings, unit files,
      repository files, release bundles, state files, logs, metrics, or support archives.

## 7. TLS and certificate gates

- [ ] TLS is enabled for public ingress, admin access, remote artifact transfer,
      NATS, databases, secret managers, and observability endpoints as required by the threat model.
- [ ] Certificate hostname/SAN verification is enabled; trust-all or plaintext fallback is forbidden.
- [ ] Certificate issuance, renewal, deployment, revocation, expiry alerting, and emergency rotation are automated.
- [ ] Private keys have restricted ownership and are not embedded in images or artifacts.
- [ ] TLS versions, cipher policy, client authentication, session behavior, and HSTS
      match organizational policy.
- [ ] The external load balancer and node proxy have a documented termination model;
      operators know which hop provides client identity and encryption.
- [ ] Rotation and expired/revoked-certificate failure paths have been tested.

## 8. Artifact and release trust gates

- [ ] Build platform artifacts only through `.github/workflows/release.yml` from
      the approved semantic-version tag; manual candidate runs are not promotable.
- [ ] Apply `INFRA_IMPL/process/RELEASE_ARTIFACT_PROMOTION.md` and run
      `scripts/verify-release-attestations.sh` on the exact downloaded archive
      before deployment.
- [ ] Verify the expected source SHA/ref, exact signer workflow, SLSA provenance,
      SPDX 2.3 attestation, closed artifact allowlist, manifest, and checksums.
- [ ] Record the workflow run, archive digest, manifest, SBOM, and attestation
      bundles in the production change record.

- [ ] Rust is pinned through `rust-toolchain.toml`; `Cargo.lock` is committed.
- [ ] Release builds use locked/frozen resolution in a controlled Linux builder.
- [ ] Required formatting, Clippy, native target, WASI target, unit, integration,
      audit, source/license, and release packaging gates pass.
- [ ] Security advisory exceptions include reachability analysis, owner, approval,
      review date, expiry, and upstream remediation tracking.
- [ ] Release output includes source revision, lockfile hash, tool versions, artifact
      hashes/sizes, SBOM, signature, and provenance/attestation.
- [ ] Nodes verify signed upgrade metadata and artifact digests before installation.
- [ ] The upgrade signer is separated from builders and production nodes and has a rotation/revocation plan.
- [ ] The previous platform binary and configuration remain available for crash-safe rollback.
- [ ] Artifact garbage collection cannot delete versions still inside the rollback window.
- [ ] Artifact upload/download authorization is tested from trusted and untrusted networks.

The current scoped bearer-token artifact flow is a bridge. Signed short-lived transfer
manifests and an explicit long-term artifact-plane identity model remain desirable
hardening for environments with mutually untrusted nodes or stronger supply-chain requirements.

## 9. Node state, storage, and recovery gates

- [ ] Each node has its own persistent redb path and Wasmtime cache/artifact space;
      no concurrent nodes mount the same writable local state.
- [ ] Local state purpose is classified: authoritative, reconstructable, cache, audit,
      or billing. Backup and retention match that classification.
- [ ] `open_failure_mode` and `integrity_failure_mode` preserve corrupt evidence and
      fail safely. Destructive recreate modes require explicit operator approval.
- [ ] Disk sizing includes redb growth, artifacts and rollback versions, code cache,
      logs, billing exports, temporary upgrade files, and forensic quarantine copies.
- [ ] Disk warning/critical thresholds use measured bytes/pages and are tested against
      the actual filesystem size.
- [ ] Backups are encrypted, access-controlled, monitored, immutable where required,
      and restored regularly into an isolated environment.
- [ ] Restore validates schema version, integrity, application/route state, encryption
      keys, and control-plane convergence.
- [ ] Loss of one node's local state is tested; the replacement either reconstructs
      safely or follows a documented restore procedure.
- [ ] Billing/audit exports have durable off-node delivery and reconciliation.
- [ ] Retention and deletion satisfy operational, security, privacy, and compliance policy.

## 10. Runtime isolation and multi-tenancy gates

- [ ] The production threat model states whether tenants and applications are trusted,
      semi-trusted, or hostile to one another.
- [ ] WASI filesystem, outbound TCP, DNS, bind, environment, file descriptor, memory,
      table, fuel, epoch, and connection policies are tested on the live execution path.
- [ ] Namespace boundaries and internal service discovery deny cross-namespace access by default.
- [ ] Node-local internal-gateway callers are attributed from a non-spoofable
      workload boundary; unresolved identity, inactive eBPF, unavailable maps,
      and consumer failure remain fail-closed and alert distinctly.
- [ ] Realm and client-specific role claims are tested positively and negatively,
      and a valid user role cannot bypass workload namespace authorization.
- [ ] The exact signed WASI/node image resolves `.internal` names through the
      production resolver path; a loopback-plus-Host test override is not accepted
      as DNS evidence.
- [ ] Every internal-mesh dependency closure uses `placement.policy = "every_node"`;
      each fully qualified `local_dependencies` entry is same-namespace and is
      deployed before its dependants on every node.
- [ ] Removing or failing a local dependency produces the documented bounded 502
      behavior, cannot cold-start from a retained artifact, alerts operators, and
      never searches or forwards to another platform node.
- [ ] Architecture and network policy explicitly prohibit cross-host `.internal`
      discovery, forwarding fallback, and workload identity. Cross-host mesh
      identity is recorded as out of scope by design, not as a missing feature.
- [ ] Applications that intentionally call a remote service use an explicit
      external endpoint with independently validated TLS, identity, authorization,
      revocation, audit, and failure semantics.
- [ ] Administrative configuration cannot accidentally widen every application's policy.
- [ ] Host-level network/cgroup/eBPF controls cover capabilities that are not fully
      authoritative inside Wasmtime host-call wrappers.
- [ ] Resource accounting is released on normal completion, trap, cancellation,
      idle pruning, node drain, and dependency failure.
- [ ] Noisy-neighbor tests prove that one application cannot exhaust node CPU, memory,
      ports, sockets, storage, or control-plane consumers for other applications.
- [ ] Runtime defaults are treated as starting points; application-specific limits
      are derived from measured work and denial-of-service objectives.
- [ ] eBPF-disabled or unavailable behavior is understood and has compensating host controls.

Optional deeper Wasmtime host/resource wrapping remains a hardening opportunity.
Whether it is a blocker depends on the production tenant threat model; document that
decision instead of assuming all policy surfaces are equally authoritative.

## 11. Capacity and performance gates

- [ ] Establish baselines for node startup, artifact download/compile, component cold
      start, warm request latency, control-event convergence, and rolling replacement.
- [ ] Load tests cover steady traffic, bursts, expensive requests, cold caches, node
      loss, deployment, and dependency degradation.
- [ ] Record CPU, resident memory, available pages, disk I/O/space, network, file
      descriptors, ports, Wasm instances, fuel, traps, and all connection pools.
- [ ] Node sizing leaves headroom above memory backpressure and disk thresholds after
      losing one node and while compiling/restarting applications.
- [ ] `health.max_memory_bytes` is explicitly below the enforced VM/container/systemd
      cgroup limit, and every application's declared instance pool fits that budget.
- [ ] `health.min_disk_free_bytes` and `health.min_disk_free_inodes` are explicit;
      steady state and rolling replacement stay above twice both reserves.
- [ ] Volume growth and inode recovery are owned, timed, and rehearsed before the hard
      readiness reserve is reached. Follow
      `INFRA_IMPL/process/RESOURCE_POLICY_PRODUCTION_VALIDATION.md`.
- [ ] Platform default fuel and epoch deadlines are benchmarked separately and do not
      reject legitimate p99 work or permit unbounded execution.
- [ ] Long-lived CLI-style service stores survive beyond the request epoch window;
      per-request WASI HTTP stores still enforce their finite wall-clock deadline.
- [ ] Wasmtime code cache is on suitable storage and bounded; pooling allocator is
      enabled only when the exact workload benchmark demonstrates a benefit.
- [ ] HAProxy/load-balancer health-check fan-out and database readiness checks are
      included in baseline traffic calculations.
- [ ] Autoscaling or manual scaling signals, cooldown, maximum size, and failure behavior are documented.
- [ ] Capacity forecasts include growth and a date/threshold for the next review.

Be precise about units. Linux/eBPF memory-pressure values in this repository use
base pages (normally 4 KiB), while Wasm component memory uses 64 KiB pages. Validate
the host page size and convert thresholds to bytes during review.

## 12. Observability, SLO, and incident-response gates

- [ ] Define platform SLOs for ingress availability/latency, deployment convergence,
      cold starts, control-plane event processing, artifact availability, and recovery.
- [ ] Logs include node ID, application/version, namespace, event/request/trace ID,
      component, severity, and timestamp without secret material.
- [ ] Metrics cover nodes, instances, routes, proxy, NATS, artifacts, redb, policy
      denials, traps, fuel/deadline exhaustion, memory/backpressure, disk, upgrades,
      authentication, and dependency health.
- [ ] Traces cross ingress, proxy, Wasm application, and supported dependencies.
- [ ] Dashboards distinguish node-local failure, cluster failure, dependency failure,
      application failure, and front-door failure.
- [ ] Alerts are tested end to end and include an owner, severity, deduplication,
      escalation, and runbook link.
- [ ] Audit logs record admin reads/writes, deployments, routes, policy/configuration
      changes, artifact access, upgrades, key rotation, recovery, and teardown.
- [ ] On-call access, incident roles, communication paths, forensic preservation,
      and status-page/customer communication are rehearsed.
- [ ] Logs, metrics, traces, audit data, and backups have documented retention and access controls.

## 13. Pre-production validation matrix

Complete these tests on production-equivalent infrastructure before opening traffic:

| Scenario | Required evidence |
|---|---|
| Clean bootstrap | All streams, consumers, node identities, routes, policies, and health become correct exactly once |
| Late node join | The node imports one valid snapshot and converges artifacts/routes/policy |
| One platform node loss | Traffic remains within SLO and placement recovers without loopback cross-wiring |
| NATS member loss | Quorum remains, events publish/consume, and no deployment state is lost |
| NATS interruption | Nodes reconnect and replay/redeliver correctly without duplicate harmful work |
| Load-balancer failure | Redundant front door or documented recovery restores traffic within RTO |
| Disk pressure/full | Alerts fire before corruption; GC and operator procedures preserve rollback/evidence |
| redb corruption | Node quarantines state and fails safely; restore/rebuild preserves evidence |
| Artifact peer loss | Another authorized source serves the exact verified digest or deployment fails safely |
| Secret/certificate rotation | Nodes remain available and old credentials are revoked |
| Database/dependency outage | Applications fail boundedly, recover, and do not create retry storms |
| Noisy/hostile workload | Policies and backpressure contain CPU, memory, network, filesystem, and FD impact |
| Platform upgrade | One-node-at-a-time rollout preserves traffic, state, routes, and control processing |
| Failed upgrade | Previous signed binary/configuration is restored within the rollback objective |
| Full cluster recovery | Documented NATS/state/secret/DNS order restores service within RTO/RPO |

- [ ] Run an extended soak covering certificate checks, token/session refresh,
      artifact GC, billing/metrics export, idle pruning, health transitions, backups,
      and at least one expected traffic cycle.
- [ ] Preserve commands, versions, timestamps, logs, dashboards, and pass/fail output as release evidence.
- [ ] Convert every failure into a fixed defect, a repeated test, or an approved time-bounded exception.

## 14. Staged production rollout

### Phase 0: freeze and approve

1. Freeze platform binary digest, configuration revision, OS image, kernel, IaC,
   NATS definition, TLS/secret policy, and rollback bundle.
2. Validate `config/production.toml` after applying production overrides without
   printing secret values.
3. Confirm all decision rules, backups, restore evidence, dashboards, alerts,
   incident roles, abort thresholds, and change approval.

### Phase 1: dependencies

1. Provision and validate highly available NATS/JetStream.
2. Provision secret/KMS, PKI, DNS, database integrations, artifact storage/transfer,
   observability, backups, and external load balancers.
3. Keep public traffic closed.

### Phase 2: initial nodes

1. Start the first node with its unique identity and private advertised endpoints.
2. Validate admin TLS/auth, storage integrity, NATS streams/consumers, artifact access,
   policy enforcement, metrics, logs, and audit events.
3. Add remaining nodes one at a time. After each join, validate snapshot selection,
   state convergence, routable peer addresses, and cluster health.

### Phase 3: platform canary

1. Deploy a harmless signed test component with restrictive capabilities.
2. Verify direct-node and front-door routing, cold/warm execution, policy denial,
   node loss, artifact retrieval, and removal.
3. Deploy one representative application and its synthetic transaction at zero or
   limited external traffic.

### Phase 4: traffic ramp

1. Add a small controlled traffic percentage or tenant cohort.
2. Observe for the declared interval and compare all abort signals with baseline.
3. Increase gradually. Stop on the first crossed abort threshold.
4. Keep previous platform and application versions available throughout the rollback window.

### Phase 5: stabilization

1. Verify every node, route, application placement, stream/consumer, backup, and alert.
2. Continue enhanced observation through at least one full operational cycle.
3. Close the change only after evidence is attached and exceptions have owners/expiry.

## 15. Platform upgrade and rollback

- [ ] Upgrade metadata and binaries are signed/provenance-verified before staging.
- [ ] The old binary/configuration remains atomically selectable.
- [ ] State-schema compatibility and backup are checked before the first node changes.
- [ ] Drain and upgrade one node at a time; require cluster and application readiness
      before continuing.
- [ ] Abort thresholds cover control-event lag, route divergence, artifacts, traps,
      latency, 5xx, backpressure, NATS, and synthetic transactions.
- [ ] A new node never advertises ready before it has converged required state and can
      serve representative application traffic.

Rollback procedure:

1. Stop further rollout and remove the failing node/version from traffic.
2. Preserve logs, state quarantine, event IDs, metrics, and the failing release bundle.
3. Restore the previous signed binary and compatible configuration atomically.
4. Restart only the affected node and require state convergence and application readiness.
5. Repeat one node at a time only if rollback itself is healthy.
6. Do not reverse a forward-only state migration by replacing the binary alone; follow
   the documented restore or forward-fix procedure.
7. Verify the original abort signals and critical synthetic transactions before closure.

## 16. Disaster-recovery gates

- [ ] Define and rehearse node replacement, availability-zone loss, NATS quorum loss,
      secret-manager/KMS outage, database outage, DNS/load-balancer loss, and complete cluster loss.
- [ ] Recovery order identifies authoritative systems and prevents stale nodes from
      overwriting newer routes, policy, or artifacts.
- [ ] Backups include the data and metadata required to decrypt and interpret restored state.
- [ ] Break-glass credentials and offline recovery documentation are secured but accessible.
- [ ] Recovery tests run in isolation and prove RTO/RPO with measured timestamps.
- [ ] Failback is planned; disaster recovery is not considered complete at initial failover.
- [ ] Post-recovery reconciliation proves node identity, routes, policies, artifacts,
      application versions, billing/audit continuity, and revoked credentials.

## 17. Production evidence record

```text
Environment / region / failure domains:
Platform release digest and source revision:
Release signature/provenance/SBOM:
OS image, kernel, and host automation revisions:
Production configuration revision (no secret values):
Node IDs, addresses, CPU/memory/disk, and placement:
NATS topology, stream replicas, and recovery evidence:
Load balancer, DNS, TLS, and firewall evidence:
Admin/artifact identity and access tests:
Secret/KMS/PKI rotation evidence:
Storage backup, restore, and integrity evidence:
Runtime policy/isolation test evidence:
Capacity, load, chaos, and soak results:
Dashboards, SLOs, alerts, and audit destinations:
Last-known-good binary/configuration and rollback result:
Disaster-recovery scenario and measured RTO/RPO:
Open exceptions with owner, approver, and expiry:
Incident commander / rollback authority:
Rollout start/end and observation window:
Final platform go/no-go approvals:
```

## 18. Known gaps to review before claiming production readiness

Re-evaluate these against the current code and threat model at every release:

- production host/IaC packaging for `wasm-node` is not supplied by the local testbed scripts;
- the local production-like topology uses one NATS microVM and one host HAProxy;
- TLS, external secrets, monitoring, backup/restore, and multi-zone placement require
  operator-provided infrastructure;
- signed short-lived artifact transfer manifests remain future hardening beyond the
  current scoped-token bridge;
- deeper policy-aware host/resource wrapping may be required for hostile multi-tenancy;
- desired-replica placement, disruption budgets, and percentage canary behavior must
  be verified as implemented rather than inferred from membership-wide test deployment;
- multi-region behavior needs separate partition and consistency validation; and
- small local rootfs images and local memory defaults are not production sizing evidence.

Do not convert an unchecked gap into a documentation promise. Either implement and
test it, provide an external control with evidence, or retain it as a release blocker.

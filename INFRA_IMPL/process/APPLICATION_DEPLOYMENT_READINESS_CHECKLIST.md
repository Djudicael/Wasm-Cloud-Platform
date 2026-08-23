# Application deployment readiness checklist

## Purpose

Use this runbook before deploying any application on Wasm Cloud Platform. It
covers application onboarding, local microVM rehearsal, production preparation,
deployment, verification, rollback, and evidence collection.

The commands in `scripts/vm/` create a local Firecracker environment only. Passing
this rehearsal proves that the application can run on the platform; it does not by
itself prove that the surrounding infrastructure is production-ready.

Qualify the underlying infrastructure separately with the
[platform production deployment checklist](./PLATFORM_PRODUCTION_DEPLOYMENT_CHECKLIST.md).

For a worked example, see
[OpenID-Connect-WASI-Hub](./OPENID_CONNECT_WASI_HUB_MICROVM_REHEARSAL.md).

### Release decision rules

Classify every checkbox as `PASS`, `FAIL`, or `EXCEPTION`. An exception must record
the risk, compensating control, owner, approver, and expiry date. Do not deploy when:

- a critical dependency, route, secret, runtime limit, or owner is unknown;
- the release artifact differs from the artifact that passed testing;
- readiness, the critical synthetic transaction, or the rollback rehearsal fails;
- a database migration lacks backup/restore evidence or version compatibility;
- production secrets are exposed through source, command lines, logs, or state files;
- critical/high reachable vulnerabilities have no approved time-bounded exception;
- required dashboards or alerts receive no current telemetry;
- the deployment cannot tolerate the declared failure scenario, such as one node loss; or
- no named operator has authority to stop or roll back the rollout.

The service owner makes the application decision. Infrastructure, security, and data
owners approve their respective gates. Absence of an objection is not approval.

## 1. Record the deployment contract

Complete this table before building or provisioning anything. An unknown value is
a failed pre-deployment gate, not a value to guess during rollout.

| Item | Required decision or evidence |
|---|---|
| Owner | Service owner and on-call contact |
| Application identity | Namespace, application name, and immutable version |
| Artifact | `wasm32-wasip2` component path, SHA-256 digest, build provenance, and retention policy |
| Entry points | Public hosts, internal hosts, route prefixes, methods, and maximum request sizes |
| Health contract | Startup, liveness, readiness, and deeper synthetic-check endpoints |
| Dependencies | Databases, NATS subjects, HTTP services, DNS names, ports, and trust boundaries |
| Configuration | Required keys, defaults, validation rules, and reload/restart behavior |
| Secrets | Secret owner, delivery mechanism, rotation process, and expiry |
| Runtime limits | Memory, fuel, epoch deadline, concurrency, outbound connections, files, and tables |
| Data | Persistence owner, migration plan, backup, restore, RPO, and RTO |
| Availability | Replica count, failure domains, disruption allowance, and scaling policy |
| SLOs | Availability, latency, error rate, saturation, and recovery objectives |
| Rollback | Last-known-good version/configuration and rollback decision owner |

Use an immutable version derived from the artifact digest and all configuration
that affects instance construction. Reusing the same application identity after
changing fuel, memory, network policy, or environment can leave existing instances
running with stale settings unless the control plane explicitly reconciles and
restarts them.

## 2. Application compatibility gates

- [ ] The application builds as a WebAssembly component for `wasm32-wasip2`.
- [ ] The component exports the expected WASI HTTP or CLI world.
- [ ] Native-only dependencies, build scripts, and system-library assumptions have
      been removed or isolated from the WASI build.
- [ ] The application validates required configuration at startup and reports
      missing or malformed values without exposing secret contents.
- [ ] Every outbound destination is documented. Network policy allows only the
      required protocols, ports, DNS names, and CIDRs.
- [ ] The application has bounded request bodies, response bodies, queues,
      concurrency, retries, and dependency timeouts.
- [ ] Retry behavior uses exponential backoff and jitter and does not multiply
      retries across the proxy, application, and database layers.
- [ ] Shutdown behavior stops accepting new requests, drains in-flight requests,
      and releases sockets, file descriptors, and connection reservations.
- [ ] The application does not depend on writable local rootfs state for durable
      data. Any local cache can be lost when an instance or node is replaced.
- [ ] Clock, random-number generation, forwarded headers, public URL, issuer URL,
      and callback URL behavior have been validated behind the real proxy shape.

For a frontend/backend system, document each component separately. Give each one
its own artifact, route, health check, resource budget, and dependency policy.

## 3. Health-check design

Use different checks for different decisions:

| Check | Purpose | Expected behavior |
|---|---|---|
| Startup | Prevent traffic before initialization completes | May be slower; succeeds only after configuration, migrations, and required initialization |
| Liveness | Decide whether the process must be restarted | Cheap and local; must not query every remote dependency |
| Readiness | Decide whether the instance can serve traffic | Includes critical dependency state, preferably cached or debounced |
| Synthetic transaction | Prove important user behavior | Runs separately at low frequency; for example, login and token issuance |

- [ ] Load-balancer checks are cheap, bounded, and safe to run continuously.
- [ ] Health checks cannot mutate customer data or create unbounded database load.
- [ ] Check interval, timeout, `fall`, and `rise` values tolerate cold starts without
      masking sustained failure.
- [ ] Check fan-out has been calculated as nodes × pools ÷ interval. For example,
      three nodes and two pools checked every two seconds generate three requests
      per second before user traffic.
- [ ] Cold-start behavior has been tested with all configured checks enabled.
- [ ] Readiness returns failure when a critical dependency cannot be used, while
      liveness remains independent enough to avoid restart loops during a remote outage.
- [ ] The verifier parses the expected response content; HTTP `200` alone is not
      sufficient when readiness contains individual dependency results.

## 4. Runtime limit and capacity gates

Fuel, epoch deadline, memory, and concurrency protect different resources. Tune and
monitor them independently.

| Limit | Protects against | How to select it |
|---|---|---|
| Fuel | Excessive Wasm instruction execution | Measure successful expensive requests and add explicit headroom |
| Epoch deadline | Excessive elapsed execution time | Set above measured p99 under contention but keep a finite upper bound |
| Guest memory | Node exhaustion and backpressure | Measure steady state, cold start, compilation, cache, and peak concurrency |
| Component memory/table limits | Guest allocation abuse | Use the smallest value that passes peak-load testing |
| Outbound connections | Socket exhaustion and dependency overload | Align with application pools and downstream capacity |
| Request/concurrency limits | Queueing and latency collapse | Derive from load tests and SLOs, then enforce admission control |

- [ ] The most expensive legitimate request has been profiled. Authentication using
      Argon2, compression, cryptography, parsing, and large queries require special attention.
- [ ] Tests prove that valid requests finish below both fuel and elapsed-time limits.
- [ ] Tests prove that runaway work is still interrupted.
- [ ] Node memory remains above the configured pressure threshold during cold start,
      steady load, failover, and artifact compilation.
- [ ] Capacity includes all colocated components rather than sizing each in isolation.
- [ ] Load testing records p50, p95, p99, error rate, fuel use, memory, CPU,
      connection counts, cold-start duration, and backpressure transitions.
- [ ] At least one-node-loss capacity is reserved when high availability is required.

Do not solve an expensive authentication request by blindly maximizing every limit.
Keep a dedicated login rate limit and verify the password-hashing parameters inside
the final microVM CPU and memory allocation.

## 5. Database and migration gates

- [ ] Schema changes use an expand-and-contract sequence compatible with the old and
      new application versions during a rolling deployment.
- [ ] Migrations are transactional where supported, idempotent, bounded, and safe to retry.
- [ ] A migration lock prevents two application replicas from applying the same
      change concurrently.
- [ ] Long-running data backfills are separated from startup and schema migration.
- [ ] The expected runtime and lock impact were measured against production-like data volume.
- [ ] A verified backup or snapshot exists before destructive or irreversible changes.
- [ ] Restore has been rehearsed and its duration meets RTO/RPO requirements.
- [ ] The rollback plan accounts for schema compatibility. Application rollback does
      not imply that a destructive schema change can be reversed.
- [ ] Database credentials are least-privilege: migration and runtime identities are
      separate when practical.
- [ ] TLS, certificate verification, pool sizing, statement timeouts, idle timeouts,
      and connection limits are configured.
- [ ] Readiness proves a usable authenticated query, not only that the TCP port is open.

Production PostgreSQL should not use the ephemeral local service VM, embedded test
credentials, or an unencrypted connection. Provide durable storage, HA/failover,
backup monitoring, restore drills, and external secret management.

## 6. Security and supply-chain gates

- [ ] Rust and frontend toolchains are pinned, manifests have committed lockfiles,
      and builds run with locked/frozen dependency resolution where supported.
- [ ] Formatting, linting, unit tests, integration tests, `cargo audit --deny warnings`,
      license/source policy, and frontend audit gates pass with documented exceptions.
- [ ] Every advisory exception has an owner, reachability analysis, expiry/review date,
      and upstream replacement plan.
- [ ] The final Wasm digest is generated after the release build and is the artifact
      that was tested, signed, promoted, and deployed.
- [ ] Artifact signature/provenance verification happens before admission.
- [ ] An SBOM and dependency inventory are retained with the release evidence.
- [ ] Secrets are not passed in command lines, committed files, artifacts, logs,
      browser bundles, state files, or HAProxy configuration.
- [ ] Production traffic uses TLS with automated renewal, secure redirect policy,
      trusted forwarded-header boundaries, and appropriate HSTS policy.
- [ ] Authentication cookies and tokens use correct `Secure`, `HttpOnly`, `SameSite`,
      issuer, audience, expiry, rotation, and revocation behavior.
- [ ] Public and administrative routes have separate authorization and rate limits.
- [ ] Egress, filesystem, DNS, bind, and environment capabilities follow least privilege.

The local rehearsal currently does not provide production TLS, an external secrets
backend, authenticated highly available NATS, or production artifact promotion.
Those controls need separate production evidence.

## 7. Routing and traffic-management gates

- [ ] Host and path bindings are unique, intentional, and checked for conflicts with
      existing applications and reserved platform endpoints.
- [ ] Route precedence is tested for `/`, nested prefixes, trailing slashes, query
      strings, encoded paths, and SPA fallback behavior.
- [ ] Frontend-to-backend URLs work through the public same-origin path when required;
      browser code does not depend on microVM-only DNS names or addresses.
- [ ] Public scheme, host, port, issuer, callback, redirect, and canonical URLs are
      correct behind every proxy layer.
- [ ] Only trusted proxies may supply forwarded headers. The application does not
      trust arbitrary client-provided `Forwarded` or `X-Forwarded-*` values.
- [ ] TLS termination, upstream protocol, HTTP/1.1 or h2c behavior, keepalive,
      connection limits, and idle/request timeouts match the application protocol.
- [ ] Streaming, WebSocket, server-sent event, upload, and download behavior is tested
      when the application uses those features.
- [ ] CORS, CSRF, CSP, cache-control, security headers, and cookie path/domain behavior
      are verified from the browser-facing origin.
- [ ] The application is stateless across replicas or its session affinity and shared
      state requirements are explicit and failure-tested.
- [ ] DNS TTL and load-balancer drain timing support the planned cutover and rollback.
- [ ] Rate limits distinguish public, authenticated, administrative, and expensive routes.
- [ ] A direct-node success and front-door failure are treated as routing evidence,
      not as a reason to increase Wasm execution limits.

## 8. Observability and operational gates

- [ ] Structured logs include timestamp, severity, node ID, namespace, application,
      immutable version, route, status, latency, and request/trace ID.
- [ ] Secret, credential, token, cookie, authorization-header, and sensitive customer
      fields are redacted at the source and in the aggregation pipeline.
- [ ] Metrics cover request rate, errors, latency, saturation, cold starts, traps,
      fuel exhaustion, epoch interruption, memory pressure, backpressure, instance
      count, connection reservations, dependency health, and deployment convergence.
- [ ] Database metrics cover connection pool use, query latency/errors, lock waits,
      migration status, storage, replication lag, backup success, and restore age.
- [ ] NATS metrics cover connectivity, consumer lag, redeliveries, stream storage,
      quorum/replica health, and control-event processing failures.
- [ ] HAProxy metrics expose backend state, check failures, queueing, retries,
      connection errors, response codes, and per-route latency.
- [ ] Distributed tracing propagates across the front door, platform proxy,
      application, and supported dependencies without trusting external trace identity blindly.
- [ ] Dashboards show SLO and capacity signals during rollout and compare the new
      version with the last-known-good version.
- [ ] Alerts are symptom/SLO based where possible, have tested delivery, named owners,
      severity, deduplication, and links to current runbooks.
- [ ] Audit events record who deployed which digest/configuration, route changes,
      secret changes, migration execution, rollback, and destructive operations.
- [ ] A low-frequency synthetic test verifies the critical user transaction separately
      from load-balancer health checks.
- [ ] Clock synchronization and consistent time zones make logs across microVMs,
      proxies, NATS, and databases correlatable.

## 9. Local validation ladder

Run commands from WSL2 or Linux. For a checkout under `/mnt/<drive>`, keep build
artifacts on the Linux filesystem:

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target
```

### Gate A: repository checks

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace --all-targets \
  --exclude http-hello-component \
  --exclude wasi-grpc-echo
cargo test --workspace \
  --exclude http-hello-component \
  --exclude wasi-grpc-echo
cargo audit --deny warnings
```

Build every WASI application explicitly because native workspace checks do not
exercise WASI-only dependency paths:

```bash
cargo build --manifest-path PATH/TO/Cargo.toml \
  --target wasm32-wasip2 \
  --release \
  --locked
sha256sum "$CARGO_TARGET_DIR/wasm32-wasip2/release/APPLICATION.wasm"
```

If a real frontend manifest and lockfile exist, use its locked install and production
build commands. Do not invent frontend steps for a repository without a frontend.

### Gate B: single-node smoke test

Use one platform node to catch artifact, WASI world, route, configuration, and basic
dependency failures quickly.

```bash
bash scripts/vm/provision-testbed.sh \
  --preset smoke \
  --nodes 1 \
  --state-file .app-smoke-state.json

bash scripts/vm/deploy-test-application.sh \
  --state-file .app-smoke-state.json \
  --app APPLICATION \
  --version IMMUTABLE_VERSION \
  --manifest PATH/TO/Cargo.toml \
  --route-host application.internal \
  --verify-path /health/ready
```

Add required service VMs before deployment. PostgreSQL is provisioned locally with
`scripts/vm/build-postgres-rootfs.sh` and
`scripts/vm/provision-postgres-service.sh`; it is not part of the platform-node count.

### Gate C: multi-node and production-shaped rehearsal

Use at least three platform nodes when verifying placement, rolling restart, proxy
routing, and node failure:

```bash
bash scripts/vm/provision-testbed.sh \
  --preset production-like \
  --nodes 3 \
  --state-file .app-rehearsal-state.json \
  --front-door haproxy \
  --front-door-bind 127.0.0.1:8088
```

The separate NATS and application service VMs do not count toward `--nodes`. Keep the
same state-file path during provision, deploy, test, and eventual teardown.

- [ ] Verify the application through every node proxy with the correct Host header.
- [ ] Verify it repeatedly through the HAProxy front door.
- [ ] Confirm host/path routing, SPA fallback, redirects, forwarded headers, and
      public URL generation.
- [ ] Run a real critical transaction, not only `/health`.
- [ ] Restart one recorded node at a time and require application readiness before
      restarting the next node.
- [ ] Test one node loss during traffic and confirm the expected error budget.
- [ ] Test dependency loss and recovery without restart loops or permanent backpressure.
- [ ] Test NATS interruption/reconnection and late-node deployment convergence.
- [ ] Test cold starts with load-balancer health checks enabled.
- [ ] Test malformed, unauthorized, oversized, slow, and rate-limited requests.
- [ ] Confirm that logs and metrics identify which node, application version, route,
      and request ID handled each request.

Preserve the environment and logs after a failure. Do not repeatedly redeploy until
the first failure has been classified.

## 10. Production infrastructure gates

The local `production-like` preset is not proof for these requirements:

- [ ] At least three appropriately sized platform nodes span the intended failure domains.
- [ ] NATS is authenticated, authorized, encrypted, persistent, monitored, backed up
      where required, and highly available.
- [ ] Databases and other stateful services meet their HA, durability, encryption,
      backup, restore, and maintenance requirements.
- [ ] Load balancers have TLS, certificate automation, application-aware pools,
      connection draining, timeouts, rate limits, and protected administration endpoints.
- [ ] Secret delivery and rotation are external to source control and deployment command lines.
- [ ] Node images, kernels, and platform binaries are pinned, checksummed, signed,
      reproducible, and rolled out one node at a time.
- [ ] Logs, metrics, traces, dashboards, alerts, and retention are operational before traffic.
- [ ] Time synchronization, DNS, firewall rules, capacity alerts, disk growth, and
      certificate expiry are monitored.
- [ ] Desired replicas, placement constraints, disruption budgets, and rescheduling
      behavior are explicit and tested.
- [ ] A staging environment matches the production network, proxy, identity, secret,
      and dependency contracts closely enough to make rehearsal meaningful.

## 11. Deployment and rollback procedure

Before deployment:

- [ ] Freeze the artifact digest and configuration revision.
- [ ] Record the last-known-good artifact and configuration.
- [ ] Confirm rollback artifacts have not been garbage-collected.
- [ ] Complete backups and migration prechecks.
- [ ] Confirm dashboards, alerts, incident channel, decision owner, and maintenance window.
- [ ] Define abort thresholds for 5xx, traps, latency, backpressure, dependency errors,
      and failed synthetic transactions.

During deployment:

1. Apply backward-compatible schema expansion first.
2. Deploy a canary or the smallest supported placement.
3. Verify startup, readiness, a real critical transaction, and telemetry.
4. Observe for a predeclared interval under representative traffic.
5. Increase placement gradually while watching abort thresholds.
6. Drain and replace nodes one at a time for a platform-binary change.
7. Stop immediately when an abort threshold is crossed; do not continue to gather
   more failures.

The current local scripts deploy to the testbed; they are not a production canary
controller. If the production control plane does not yet support percentage or
placement-based rollout, implement that capability or use an external traffic gate
before treating gradual rollout as available.

Rollback:

1. Stop further rollout and preserve logs, metrics, event IDs, and failing responses.
2. Remove the new version from traffic or restore the previous route binding.
3. Redeploy the recorded last-known-good artifact and its compatible configuration.
4. Verify readiness and the critical synthetic transaction.
5. Restore the database only when the migration plan explicitly requires it; avoid
   destructive rollback while healthy writers are active.
6. Confirm recovery against the same abort thresholds and open a post-incident review.

## 12. Post-deployment verification

- [ ] Every intended node/placement reports the expected immutable version.
- [ ] Front-door and direct-node checks agree.
- [ ] Critical user transactions pass, including authentication where applicable.
- [ ] Error, trap, saturation, and latency metrics remain within the declared SLO.
- [ ] No fuel exhaustion, epoch interruption, memory pressure, connection leak,
      route flapping, or repeated cold-start loop is present.
- [ ] Database migrations and schema version are correct; replica lag and pool usage
      remain healthy.
- [ ] Logs contain no secrets and include request, application, version, and node identity.
- [ ] Alerts were exercised or otherwise proven to receive current telemetry.
- [ ] The deployment record contains artifact digest, configuration revision,
      migration version, operator, timestamps, test evidence, and rollback target.

Observe for at least the period needed to cover cold starts, health-check transitions,
token/session refresh, scheduled work, and the expected traffic cycle. A successful
first request is not a completed deployment.

## 13. Evidence record template

Copy this block into the release record:

```text
Application / namespace:
Owner / on-call:
Artifact version and SHA-256:
Source revision:
Toolchain and lockfile revisions:
Configuration revision:
Runtime limits (fuel/deadline/memory/concurrency/connections):
Routes and health endpoints:
Dependencies and policy grants:
Database migration version:
Backup and restore evidence:
Node count / placement / failure domains:
Local rehearsal state file and topology:
Direct-node verification results:
Front-door verification results:
Load/chaos/security test results:
Dashboards and alerts:
Last-known-good rollback target:
Abort thresholds and decision owner:
Deployment start/end timestamps:
Outstanding exceptions, owners, and expiry dates:
Final go/no-go approval:
```

## 14. Local teardown

Destroy a local testbed only after testing is complete and the user explicitly asks:

```bash
bash scripts/vm/destroy-testbed.sh \
  --state-file .app-rehearsal-state.json
```

Use the exact recorded state file. Never stop Firecracker processes, remove TAP
devices, or delete bridges by broad name or process matching.

# Single-host production validation final decision

Run ID: `single-host-2026-08-23--2026-08-29`

Decision time: `2026-08-29T21:42:52Z`

Platform release decision (reconciled 2026-09-06): **SIGNED RELEASE CANDIDATE
VERIFIED — NO PLATFORM-WIDE NO-GO**

Deployment decision: **CONDITIONAL ON THE SELECTED DEPLOYMENT PROFILE**

Platform source decision: **PASS FOR THE SUPPORTED SINGLE-TRUST-DOMAIN MODEL**

Release candidate decision: **PASS WITH EXPLICIT, TIME-BOUNDED UPSTREAM
DEPENDENCY EXCEPTIONS**

Local rehearsal result: **PASS WITH DOCUMENTED SINGLE-HOST LIMITS**

The three-node Firecracker platform, PostgreSQL-backed OIDC application,
HAProxy routing, failure recovery, eBPF monitoring, capacity envelope,
upgrade/rollback, and application-level database restore all worked within the
documented single-host boundary. That is strong engineering evidence, but it is
not permission to promote this checkout or topology to production. The
[Phase 10 reconciliation](PHASE_10_RECONCILIATION.md) separates closed platform
source work from release-specific and operator-owned production evidence. The
signed workflow artifact and GitHub attestations are the authoritative source
for the candidate's exact commit and digest; run-specific summaries may be
retained locally without making them part of the source commit.

## What the rehearsal proved

- Three 2-vCPU/2-GiB platform nodes converged through a separate NATS microVM.
- PostgreSQL 17.11 migrations were repeatable and the OIDC backend reported
  database readiness through every node.
- The focused Chromium login/dashboard suite passed before and after rolling
  node replacement, upgrade rollback, and restored-database cutover.
- Controlled node, network, NATS, PostgreSQL, storage, CPU, memory, FD, and
  connection faults had bounded behavior and recovered.
- The configured front-door envelope sustained the representative 5 requests/s
  mix, including N-1 operation. This is a local policy/host result, not a VPS or
  production capacity guarantee.
- eBPF attribution, deterministic probes, block-I/O fields, ring pressure,
  degraded modes, cleanup, and overhead met their local thresholds. The final
  overhead comparison used 80,000 requests per mode and observed 0.12%
  throughput loss, +1.11 ms p99, and zero failures or event loss.
- Upgrade/rollback preserved 450/450 HTTP 200 responses at 5 requests/s and
  rejected a malformed candidate without moving the public route to it.
- Disaster recovery matched 26 snapshot tables by count and logical-content
  hash, retained 11/11 migrations, and completed the restored browser journey
  in 52.053 seconds.
- At final capture, every direct-node and public OIDC readiness check returned
  HTTP 200 with `database=ok`; Prometheus reported 10/10 targets, zero alerts,
  and three active non-degraded eBPF nodes.

## Promotion gates and ownership

The historical `P1` labels below identify gates that prevent opening production
traffic without evidence. They do not mean every row is an unresolved platform
source defect. P10 source remediation is complete for the supported production
model. The remaining work is either proof for the exact signed candidate or an
operator/provider integration gate. P10-06 is testbed-only. PostgreSQL, Vault,
HAProxy, Prometheus/telemetry backends, and the OIDC Hub remain fixtures or
external services, not platform release components.

| ID | Severity | Gate | Required closure evidence |
|---|---|---|---|
| P10-01 | SIGNED NON-PROMOTABLE CANDIDATE PASS / GA TAG NOT EXECUTED | The current candidate passed frozen native/WASI/eBPF builds, closed artifact staging, policy audit, deterministic packaging, SPDX 2.3, GitHub OIDC/Sigstore provenance and SBOM attestations, pre-publication verification, upload, download, and independent admission. Manifest schema 3 binds the exact documented audit exceptions. Use the workflow artifact metadata rather than this source document for the immutable candidate identity. | The platform-wide candidate gate is closed. A GA release remains a separate approval: create the semantic-version tag from the reviewed source, rerun the workflow, retain tagged bytes and attestations, and independently admit them. |
| P10-02 | P1, SOURCE REMEDIATED + REAL VAULT MICROVM PASS / PRODUCTION EVIDENCE PENDING | Production admission rejects insecure defaults and exportable seal roots. Nodes support pinned Vault Transit/AWS KMS HMAC roots, private Vault CA bundles, real base64 HMAC responses, controlled envelope rewrap, encrypted application-secret rotation, durable revocation, and warm-instance invalidation. A sealed Vault 1.21.4 Firecracker VM with TLS, private CA, least-privilege CIDR-bound AppRoles, response-wrapped SecretIDs, a non-exportable 256-bit HMAC key, and socket audit passed actual-node initialization, version 3-to-4 rotation, KEK/transport rewrap, current-only restart, sealed outage, recovery, authorization-negative checks, audit correlation, checksums, and sentinel scanning. | Local Vault protocol compatibility is proven. Production still requires the real workload identity and renewal, HA Vault or KMS/HSM, production PKI, durable audit retention, application-secret rotation/delete acknowledgement from every node, admin new-token success/old-token 401, and candidate-wide log/trace/audit/CI scans. See [Vault evidence](../2026-08-30-single-host/P10-02-vault-microvm/README.md) and [runbook](../../../VAULT_TRANSIT_MICROVM_VALIDATION.md). |
| P10-03 | P1, SOURCE REMEDIATED + LOCAL MICROVM PASS / PRODUCTION CONTROLS PENDING | Nodes now activate OTLP safely, propagate W3C context, identify node/environment/exact deployment, and emit correlated structured request records. A local Collector separates operational/audit streams and uses a bounded 2048-batch disk queue. Queryable OIDC traces correlate platform handling with PostgreSQL client activity. Tempo and Collector interruption alerts fired while the application remained available; the Tempo queue recovered. | The immediate Collector outage proved an explicit loss window because the node exporter has no WAL. Production needs a supervised local Collector per host or a node-side WAL/accepted-loss policy, authenticated TLS, independently operated immutable audit and off-host telemetry retention, capacity/sampling policy, and the exhaustive drop/notification gates in P10-04. See [telemetry evidence](../2026-08-30-single-host/P10-03-telemetry/README.md) and [runbook](../../../PRODUCTION_TELEMETRY_VALIDATION.md). |
| P10-04 | P1, SOURCE REMEDIATED + LOCAL MICROVM PASS / PRODUCTION RECEIVER EVIDENCE PENDING | The current five files contain 36 alerts. The 2026-09-05 P10-08 rerun required all 30 always-present metric names and included configuration-aligned disk/inode hard reserves plus an actionable disk-headroom warning. Seven operational categories each produced one deduplicated firing and one resolved delivery after three identical submissions. | Production must attach the real authenticated on-call receiver, assign owners and reachable runbooks, validate inhibition/silence/repeat policy and Alertmanager HA, and preserve provider-side firing/resolution evidence. See [P10-08 evidence](../2026-09-05-single-host/P10-08-resource-policy/README.md), [alerting evidence](../2026-08-30-single-host/P10-04-alerting/README.md), and [runbook](../../../PRODUCTION_ALERTING_VALIDATION.md). |
| P10-05 | P1, SOURCE REMEDIATED + OIDC MICROVM PASS / PRODUCTION PIPELINE EVIDENCE PENDING | The platform strips untrusted identity/trace headers and its local HAProxy logs paths without query values. The OIDC application validates registered redirects before trusting them for error responses and logs only request paths. A state-scoped sentinel test passed across the selected HAProxy, node, audit/operational, response, and Tempo artifacts; invalid admin credentials were rejected on every node, and exact application attribution did not cross. OIDC tests cover authorization-code replay/expiry and session expiry. | Roll the signed candidate platform image containing the final sanitizer; run the exact signed application security suite; scan the real ingress/WAF/CDN, telemetry, audit, dead-letter, support, crash, and CI retention paths; and prove RP state mismatch plus ID-token nonce mismatch. See [P10-05 evidence](../2026-08-30-single-host/P10-05-security/README.md) and the [runbook](../../../PLATFORM_REDACTION_AND_OIDC_SECURITY_VALIDATION.md). |
| P10-05A | P1, API GATEWAY OIDC LOCAL PASS / PRODUCTION IDP AND PKI EVIDENCE PENDING | Three microVM nodes retained strict public issuer/audience validation while fetching JWKS through a private bridge route. A WASI example proved public, authenticated, and scope-protected endpoints on every node and HAProxy. Valid tokens/scopes passed; missing, malformed, wrong-audience, expired, and missing-scope cases returned 401/403 as designed. Separate admin/artifact deployment ports are now supported. | Repeat with the signed candidate, production OIDC provider, HTTPS/PKI, key rotation, provider failure, positive/negative roles, multi-host routing, and production telemetry. The Hub is a test fixture rather than a required platform service. See [evidence](../2026-08-30-single-host/P10-05-api-gateway-oidc/README.md) and [runbook](../../../API_GATEWAY_OIDC_MICROVM_VALIDATION.md). |
| P10-05B | P1, NODE-LOCAL INTERNAL MESH CONTRACT PASSED LOCALLY / SIGNED IMAGE EVIDENCE PENDING | Three microVM nodes used literal `.internal` DNS, explicit `every_node` dependency placement, and eBPF caller identity. They passed 24 role/negative/namespace checks plus 288/288 concurrent calls. Removing the local dependency returned 502 on every node without retained-artifact cold start or cross-node fallback; redeployment restored 200. TCP-close accounting and the erroneous 30-second CLI-service lifetime were remediated and exercised. | Repeat from the exact signed production node image/kernel on every production node class, with mandatory eBPF/alerting and production IdP HTTPS/PKI rotation/failure. Cross-host mesh identity is out of scope by design; verify that network and placement policy preserve that boundary. See [evidence](../2026-08-30-single-host/P10-05-node-local-mesh-production/README.md) and [runbook](../../../INTERNAL_MESH_OIDC_ROLE_VALIDATION.md). |
| P10-06 | TESTBED PASS / REMOVED AS PLATFORM PRODUCTION BLOCKER | The checksum-pinned Linux 6.18.48 and Firecracker 1.16.1 unit is a reproducible microVM test fixture. Its canary returned HTTP 200, connected NATS, and attached 7/7 platform eBPF programs with active=1/degraded=0. The WSL `spec_rstack_overflow` result describes only that host/virtualization layer. The platform release no longer builds, packages, attests, or verifies the Firecracker kernel, and the standalone kernel workflow was removed. | No platform-source closure is required. Each production operator must validate the chosen VPS/host OS, kernel patching, resource controls, and optional eBPF prerequisites. KVM/Firecracker checks apply only when that deployment deliberately uses Firecracker. See [testbed policy](../../../VM_TESTBED_KERNEL_VALIDATION.md) and [P10-06 testbed evidence](../2026-09-01-single-host/P10-06-kernel/README.md). |
| P10-07 | P1, SOURCE REMEDIATED + LOCAL MICROVM PASS / PRODUCTION TIME AND DATABASE CONTROLS PENDING | A fresh schema-5 PostgreSQL VM passed fail-closed startup, a 90-second exact-VMM suspension, explicit degraded metrics, authenticated one-time source reset/burst recovery, bounded direct/Prometheus skew, and firing/resolved clock-source alerts. The current 35-rule alert contract passed live. The OIDC recovery rerun verified 26 tables, 11/11 migrations, six Playwright checks, 6.928-second restore, 44.844-second application readiness, 53.154-second full journey, and 1.78-second database skew. Final readiness was `database=ok`, two time sources were usable, and no alert remained. PostgreSQL is an optional application dependency, not a platform component. | Local behavior is proven. Production separately requires at least three independent operator-controlled sources, authenticated time where available, host/guest offset SLOs, real paging, and the database operator's HA, scheduled/off-host backup, immutable retention, and KMS/HSM controls. See [P10-07 evidence](../2026-09-01-single-host/P10-07-postgres-clock/README.md). |
| P10-08 | P1, SOURCE REMEDIATED + LOCAL MICROVM PASS / PRODUCTION SIZING EVIDENCE PENDING | Admission now enforces per-instance caps and requires each declared application pool to fit the configured node budget on deploy, config update, restore, and spawn. Schema-14 2-GiB test nodes use a 1,536-MiB budget and 512-MiB/10,000-inode hard reserves; all three remained healthy at 18-19% memory after a 600/600 OIDC soak. A synthetic 2,048-MiB pool was absent from every node. Disk alerts now compare emitted configured reserves and warn below twice the hard reserve. | Apply the runbook to the exact signed candidate and selected production node/volume classes. Preserve enforced cgroup limits, longer representative and N-1 load evidence, tested volume growth/inode recovery, and real firing/resolved paging evidence. See [P10-08 evidence](../2026-09-05-single-host/P10-08-resource-policy/README.md) and [runbook](../../../RESOURCE_POLICY_PRODUCTION_VALIDATION.md). |
| P10-09 | P1, SOURCE REMEDIATED + LOCAL PLATFORM CONTRACT PASS / OPERATOR INTEGRATION EVIDENCE PENDING | Nodes, `wasm-ctl`, and deploy ingress now support explicit private-CA and NATS mTLS inputs. Production rejects plaintext NATS and plaintext advertised artifact URLs. Proxy, admin, deploy-ingress, and artifact HTTPS passed; plaintext and missing material failed closed. A real node reported a stopped mTLS NATS service through HTTP 503 readiness and recovered to 200 after restart. Runtime testing also corrected the Rustls provider bootstrap and separated cleartext h2c from TLS/ALPN in Pingora. | Repeat with the exact signed candidate and production-equivalent PKI, NATS accounts/subjects and HA cluster, certificate/credential rotation and revocation, real load balancer, failure domains, and paging. PostgreSQL HA/backups, KMS/HSM/Vault service operation, immutable retention, and provider infrastructure remain operator-owned rather than platform components. See [evidence](../2026-09-05-single-host/P10-09-platform-integration/README.md) and [runbook](../../../PLATFORM_TLS_AND_NATS_PRODUCTION_VALIDATION.md). |
| P10-10 | P1, SOURCE CONTRACT REMEDIATED + SINGLE-NODE MICROVM PASS / SIGNED HOST-CLASS EVIDENCE PENDING | Production now admits only `runtime.isolation_mode = "single-trust-domain"` and mandatory eBPF. The schema-15 fixture launches `wasm-node` in a dedicated cgroup-v2 cgroup; block-I/O and memory-pressure probes reject other cgroups, while application-aware probes use registered runtime TIDs. A rolled node correlated guest cgroup ID 21 with all BPF maps, attached 7/7 programs, restored both OIDC components, and kept all three nodes healthy. Buffered kernel writeback is explicitly not per-application accounting, and hostile colocated tenants remain unsupported. | Repeat the [workload-isolation runbook](../../../PLATFORM_WORKLOAD_ISOLATION_VALIDATION.md) with the exact signed candidate on every production node class. Preserve dedicated-cgroup, mandatory eBPF/readiness/alerting, sustained TID attribution, and lifecycle-cleanup evidence. If mutually untrusted applications must share a host, isolate them on separate node processes/VMs; process-per-application support requires a new implementation and security campaign. See [local evidence](../2026-09-05-single-host/P10-10-workload-isolation/README.md). |

The 2026-09-06 candidate policy audit has zero unexcepted vulnerabilities or
warnings across 747 locked dependencies. It is not an exception-free result:
seven upstream findings remain explicitly accepted and are bound into the
release manifest. Their reachability, owners, review deadline, and removal
conditions are recorded in
[`DEPENDENCY_SECURITY_EXCEPTIONS.md`](../../../DEPENDENCY_SECURITY_EXCEPTIONS.md).
The avoidable `RUSTSEC-2023-0071` RSA advisory was removed by switching JWT
verification to AWS-LC and replacing test-only RSA generation with a fixed test
fixture. The prior Wasmtime 47.0.3 advisories remain resolved in 47.0.4.

## Checks that cannot be reconstructed after the run

The pre-run firewall, listening-port, bridge, TAP, and host-route snapshot was
not captured. The final state is documented, but a post-run snapshot cannot be
used as evidence of the pre-run state. This gate is `NOT VALIDATED` and must be
captured before the next rehearsal.

## Required next sequence

1. Preserve and review the independently verified candidate digest,
   provenance, signatures, SBOM, and explicit dependency exceptions.
2. Deploy that exact digest to production-equivalent staging using the selected
   operator infrastructure.
3. Rerun the applicable platform gates on every chosen node class. PostgreSQL
   and OIDC checks apply only when the deployed workload requires them.
4. Obtain an independent review of the new evidence and approve any explicit,
   time-bounded exceptions.
5. Execute `02_TWO_PHYSICAL_HOST_PRODUCTION_VALIDATION.md` only if the selected
   deployment claims host-failure tolerance or cross-host availability. It is
   not a platform release prerequisite.
6. Validate the provider controls applicable to the selected deployment profile,
   such as PKI, secret manager, load balancer, storage,
   backup/KMS, and availability behavior in staging.
7. Issue a deployment decision for that exact digest and environment. If it is
   approved for GA, create the semantic-version tag and repeat the workflow and
   independent admission on tagged bytes.

## Teardown status

The environment was explicitly authorized for teardown and destroyed on
2026-08-30 with the canonical state-scoped script. The post-teardown audit
confirmed the recorded platform, NATS, PostgreSQL, Vault, HAProxy, observability,
network, state, and runtime-secret resources were absent and the local service
ports were closed. The retained evidence remains available for the later
production-decision review.

This teardown record applies to the historical 2026-08-29 state. The later
`.prod-validation-p10-08-state.json` continuation environment is separate and
was intentionally left running pending explicit user authorization to destroy
that exact recorded state.

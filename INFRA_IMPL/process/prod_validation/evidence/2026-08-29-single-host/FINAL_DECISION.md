# Single-host production validation final decision

Run ID: `single-host-2026-08-23--2026-08-29`

Decision time: `2026-08-29T21:42:52Z`

Decision: **NO-GO FOR PRODUCTION PROMOTION**

Local rehearsal result: **CORE PLATFORM FUNCTIONAL PASS WITH OPEN PRODUCTION GATES**

The three-node Firecracker platform, PostgreSQL-backed OIDC application,
HAProxy routing, failure recovery, eBPF monitoring, capacity envelope,
upgrade/rollback, and application-level database restore all worked within the
documented single-host boundary. That is strong engineering evidence, but it is
not permission to promote this checkout or topology to production.

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

## Production blockers

| ID | Severity | Gate | Required closure evidence |
|---|---|---|---|
| P10-01 | P1, REMEDIATED IN SOURCE / TAG EVIDENCE PENDING | The former release job was partial. It now enforces an exact clean source SHA, frozen native/WASI/eBPF builds, a closed artifact allowlist, deterministic packaging, SPDX 2.3, GitHub OIDC/Sigstore SLSA and SBOM attestations, pre-publication verification, and fail-closed operator admission. Manual runs are non-promotable. Local positive/reproducibility/tamper tests pass. | Run the workflow from the approved semantic-version tag, preserve its immutable digest and attestation bundles, then have an independent operator run `scripts/verify-release-attestations.sh` on the downloaded bytes. Only that release-specific evidence closes the P1 gate. |
| P10-02 | P1, SOURCE REMEDIATED + REAL VAULT MICROVM PASS / PRODUCTION EVIDENCE PENDING | Production admission rejects insecure defaults and exportable seal roots. Nodes support pinned Vault Transit/AWS KMS HMAC roots, private Vault CA bundles, real base64 HMAC responses, controlled envelope rewrap, encrypted application-secret rotation, durable revocation, and warm-instance invalidation. A sealed Vault 1.21.4 Firecracker VM with TLS, private CA, least-privilege CIDR-bound AppRoles, response-wrapped SecretIDs, a non-exportable 256-bit HMAC key, and socket audit passed actual-node initialization, version 3-to-4 rotation, KEK/transport rewrap, current-only restart, sealed outage, recovery, authorization-negative checks, audit correlation, checksums, and sentinel scanning. | Local Vault protocol compatibility is proven. Production still requires the real workload identity and renewal, HA Vault or KMS/HSM, production PKI, durable audit retention, application-secret rotation/delete acknowledgement from every node, admin new-token success/old-token 401, and candidate-wide log/trace/audit/CI scans. See [Vault evidence](../2026-08-30-single-host/P10-02-vault-microvm/README.md) and [runbook](../../../VAULT_TRANSIT_MICROVM_VALIDATION.md). |
| P10-03 | P1, SOURCE REMEDIATED + LOCAL MICROVM PASS / PRODUCTION CONTROLS PENDING | Nodes now activate OTLP safely, propagate W3C context, identify node/environment/exact deployment, and emit correlated structured request records. A local Collector separates operational/audit streams and uses a bounded 2048-batch disk queue. Queryable OIDC traces correlate platform handling with PostgreSQL client activity. Tempo and Collector interruption alerts fired while the application remained available; the Tempo queue recovered. | The immediate Collector outage proved an explicit loss window because the node exporter has no WAL. Production needs a supervised local Collector per host or a node-side WAL/accepted-loss policy, authenticated TLS, independently operated immutable audit and off-host telemetry retention, capacity/sampling policy, and the exhaustive drop/notification gates in P10-04. See [telemetry evidence](../2026-08-30-single-host/P10-03-telemetry/README.md) and [runbook](../../../PRODUCTION_TELEMETRY_VALIDATION.md). |
| P10-04 | P1, SOURCE REMEDIATED + LOCAL MICROVM PASS / PRODUCTION RECEIVER EVIDENCE PENDING | Native tools validate the live Prometheus and Alertmanager configurations and all four tracked rule files. Representative inputs cross all 32 alert thresholds; every loaded expression succeeds against live Prometheus. Seven required operational categories each produced one deduplicated firing and one resolved delivery after three identical submissions. Effective admin-auth state is now explicit, per-minute WASI policy thresholds are correct, and elevated HAProxy 5xx rate is covered. | Production must attach the real authenticated on-call receiver, assign owners and reachable runbooks, validate inhibition/silence/repeat policy and Alertmanager HA, and preserve provider-side firing/resolution evidence. See [alerting evidence](../2026-08-30-single-host/P10-04-alerting/README.md) and [runbook](../../../PRODUCTION_ALERTING_VALIDATION.md). |
| P10-05 | P1, SOURCE REMEDIATED + OIDC MICROVM PASS / PRODUCTION PIPELINE EVIDENCE PENDING | The platform strips untrusted identity/trace headers and its local HAProxy logs paths without query values. The OIDC application validates registered redirects before trusting them for error responses and logs only request paths. A state-scoped sentinel test passed across the selected HAProxy, node, audit/operational, response, and Tempo artifacts; invalid admin credentials were rejected on every node, and exact application attribution did not cross. OIDC tests cover authorization-code replay/expiry and session expiry. | Roll the signed candidate platform image containing the final sanitizer; run the exact signed application security suite; scan the real ingress/WAF/CDN, telemetry, audit, dead-letter, support, crash, and CI retention paths; and prove RP state mismatch plus ID-token nonce mismatch. See [P10-05 evidence](../2026-08-30-single-host/P10-05-security/README.md) and the [runbook](../../../PLATFORM_REDACTION_AND_OIDC_SECURITY_VALIDATION.md). |
| P10-05A | P1, API GATEWAY OIDC LOCAL PASS / PRODUCTION IDP AND PKI EVIDENCE PENDING | Three microVM nodes retained strict public issuer/audience validation while fetching JWKS through a private bridge route. A WASI example proved public, authenticated, and scope-protected endpoints on every node and HAProxy. Valid tokens/scopes passed; missing, malformed, wrong-audience, expired, and missing-scope cases returned 401/403 as designed. Separate admin/artifact deployment ports are now supported. | Repeat with the signed candidate, production OIDC provider, HTTPS/PKI, key rotation, provider failure, positive/negative roles, multi-host routing, and production telemetry. The Hub is a test fixture rather than a required platform service. See [evidence](../2026-08-30-single-host/P10-05-api-gateway-oidc/README.md) and [runbook](../../../API_GATEWAY_OIDC_MICROVM_VALIDATION.md). |
| P10-05B | P1, NODE-LOCAL INTERNAL MESH CONTRACT PASSED LOCALLY / SIGNED IMAGE EVIDENCE PENDING | Three microVM nodes used literal `.internal` DNS, explicit `every_node` dependency placement, and eBPF caller identity. They passed 24 role/negative/namespace checks plus 288/288 concurrent calls. Removing the local dependency returned 502 on every node without retained-artifact cold start or cross-node fallback; redeployment restored 200. TCP-close accounting and the erroneous 30-second CLI-service lifetime were remediated and exercised. | Repeat from the exact signed production node image/kernel on every production node class, with mandatory eBPF/alerting and production IdP HTTPS/PKI rotation/failure. Cross-host mesh identity is out of scope by design; verify that network and placement policy preserve that boundary. See [evidence](../2026-08-30-single-host/P10-05-node-local-mesh-production/README.md) and [runbook](../../../INTERNAL_MESH_OIDC_ROLE_VALIDATION.md). |
| P10-06 | P1 | The local tiny kernel is not approved for production hardening; historical serial evidence identified missing speculative-execution mitigations. | Kernel based on a maintained Firecracker production config, vulnerability/mitigation audit, signed image pipeline, patch policy, and host-specific validation. |
| P10-07 | P1 | The live PostgreSQL VM still uses schema 3 and was approximately 12,399 seconds behind after host suspend. Schema 4 with mandatory Chrony built offline but was not used to replace the live database. | Fresh environment booted from schema 4, verified time-source/offset alerts, suspend/resume or clock-fault test, backup/restore rerun, and production redundant time sources. |
| P10-08 | P1 | Resource policy is inconsistent with VM sizing: the node memory ceiling exceeds the 2-GiB VM boundary and the 2-GiB disks remain below the configured warning envelope. | Explicit per-node/cgroup budgets below hard VM limits, production disk sizing/growth policy, sustained load validation, and actionable headroom alerts. |
| P10-09 | P1 | Production TLS/PKI, external secrets, highly available NATS/PostgreSQL, off-host backups, KMS/HSM recovery, immutable retention, and managed load-balancer behavior are outside this local topology. | Operator-owned service designs and staged evidence, followed by the two-physical-host plan and provider-specific validation. |
| P10-10 | Conditional P1 | eBPF block-I/O attribution can be system-wide. TCP-close events now release persistent WASI CLI outbound reservations, but that accuracy requires mandatory eBPF. This is not a tenant-grade block-I/O isolation claim. | Dedicated cgroup/process boundaries and cross-tenant non-observation tests before multi-tenant use; require eBPF when active connection limits are enforced, or document and enforce a single-trust-domain deployment constraint. |

The separately tracked `proc-macro-error2 2.0.1` future-incompatibility notice is
release debt, but the final `cargo audit --deny warnings` run passed against the
current advisory database. It is not being misclassified as a current RustSec
vulnerability in this decision.

## Checks that cannot be reconstructed after the run

The pre-run firewall, listening-port, bridge, TAP, and host-route snapshot was
not captured. The final state is documented, but a post-run snapshot cannot be
used as evidence of the pre-run state. This gate is `NOT VALIDATED` and must be
captured before the next rehearsal.

## Required next sequence

1. Close P10-01 through P10-08 in code and release automation.
2. Provision a fresh single-host environment from the production candidate,
   including PostgreSQL image schema 4, and rerun only the affected gates.
3. Obtain an independent review of the new evidence and approve any explicit,
   time-bounded exceptions.
4. Execute `02_TWO_PHYSICAL_HOST_PRODUCTION_VALIDATION.md` with the same signed
   artifacts, application journeys, dashboards, alerts, and load profiles.
5. Validate provider-specific PKI, secret manager, load balancer, storage,
   backup/KMS, and availability behavior in staging.
6. Issue a new production decision. This record cannot be converted into a
   production approval by editing the verdict without new evidence.

## Teardown status

The environment was explicitly authorized for teardown and destroyed on
2026-08-30 with the canonical state-scoped script. The post-teardown audit
confirmed the recorded platform, NATS, PostgreSQL, Vault, HAProxy, observability,
network, state, and runtime-secret resources were absent and the local service
ports were closed. The retained evidence remains available for the later
production-decision review.

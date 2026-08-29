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
| P10-03 | P1 | Phase 5 logs and distributed traces are not implemented end to end. The Collector is healthy, but the node OTLP setting is not wired and there is no bounded log shipper or correlated HAProxy-to-PostgreSQL trace. | Structured schema, bounded durable buffering, separated audit stream, trace propagation, interruption/recovery tests, drop alerts, and one queryable OIDC trace. |
| P10-04 | P1 | Alert-rule unit coverage, every-expression live queries, notification receipt, resolution, and deduplication were not completed for the entire rule set. | `promtool test rules`, live query audit, deterministic firing/resolution for every required alert, and notification/dedup evidence. |
| P10-05 | P1 | Negative OIDC security cases remain open: invalid redirect URI, expired state/nonce/session, and systematic proof that secrets never enter logs/traces. | Focused serial security tests with redacted evidence and expiry/time-skew cases. |
| P10-06 | P1 | The local tiny kernel is not approved for production hardening; historical serial evidence identified missing speculative-execution mitigations. | Kernel based on a maintained Firecracker production config, vulnerability/mitigation audit, signed image pipeline, patch policy, and host-specific validation. |
| P10-07 | P1 | The live PostgreSQL VM still uses schema 3 and was approximately 12,399 seconds behind after host suspend. Schema 4 with mandatory Chrony built offline but was not used to replace the live database. | Fresh environment booted from schema 4, verified time-source/offset alerts, suspend/resume or clock-fault test, backup/restore rerun, and production redundant time sources. |
| P10-08 | P1 | Resource policy is inconsistent with VM sizing: the node memory ceiling exceeds the 2-GiB VM boundary and the 2-GiB disks remain below the configured warning envelope. | Explicit per-node/cgroup budgets below hard VM limits, production disk sizing/growth policy, sustained load validation, and actionable headroom alerts. |
| P10-09 | P1 | Production TLS/PKI, external secrets, highly available NATS/PostgreSQL, off-host backups, KMS/HSM recovery, immutable retention, and managed load-balancer behavior are outside this local topology. | Operator-owned service designs and staged evidence, followed by the two-physical-host plan and provider-specific validation. |
| P10-10 | Conditional P1 | eBPF block-I/O attribution can be system-wide and persistent WASI CLI connection accounting is instance-lifetime conservative. These are not tenant-grade isolation claims. | Dedicated cgroup/process boundaries and cross-tenant non-observation tests before multi-tenant use; otherwise document and enforce a single-trust-domain deployment constraint. |

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

The environment was deliberately left running. No Phase 10 teardown was
performed, and the current state/companion files, recorded PIDs, TAP devices,
bridge, HAProxy process, service containers, and microVMs remain available for
interactive inspection. Destruction still requires explicit user authorization.

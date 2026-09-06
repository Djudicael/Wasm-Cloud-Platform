# Phase 10 readiness reconciliation

Reconciled: `2026-09-06`

This record separates platform-source readiness from local testbed evidence and
operator-owned production admission. A dependency used by the rehearsal is not
automatically a component that this repository must deploy or operate.

## Decisions

| Decision | Result | Meaning |
|---|---|---|
| Supported platform source contract | **PASS** | No unresolved P0/P1 source defect is known for the documented `single-trust-domain` production model. The source is eligible to become a signed release candidate. |
| Single-host Firecracker rehearsal | **PASS WITH DOCUMENTED LIMITS** | Platform behavior, failures, recovery, observability output, eBPF, upgrade/rollback, and representative WASI applications passed within one physical host. |
| Platform release legitimacy | **PASS — SIGNED RELEASE CANDIDATE VERIFIED** | The current candidate workflow result is the authoritative source for its exact commit, digest, and attestations. Manual candidates are deliberately non-promotable. A second physical host is not a platform release prerequisite. |
| A particular production deployment | **CONDITIONAL ON ITS DECLARED PROFILE** | Do not open traffic from this working tree or local testbed artifacts. A baseline deployment needs the signed candidate and its applicable operator gates. Two-host evidence is required only when that deployment claims host-failure tolerance or cross-host availability. |
| Hostile same-node multitenancy | **NOT SUPPORTED** | The admitted production model is one trust domain per `wasm-node`. Mutually untrusted workloads require separate node processes, VMs, or hosts. |

There is no remaining platform-wide `NO-GO` caused by the optional two-host
exercise. The signed workflow artifact and GitHub attestations preserve the
exact candidate identity outside the source commit; embedding a commit's own
SHA in that commit would be self-referential. Local evidence still cannot prove
the configuration of a future production environment, so deployment decisions
remain profile-specific.

## Ownership matrix

| Gate | Platform result | Remaining evidence owner |
|---|---|---|
| P10-01 release supply chain | Signed non-promotable candidate and independent admission pass; use the workflow artifact metadata for its exact immutable identity. | Release owner: after staging approval, create the approved semantic-version tag and repeat retention/admission for tagged bytes. This is GA promotion, not a platform-source blocker. |
| P10-02 secret-root integration | Vault Transit and AWS KMS HMAC client contracts, fail-closed admission, rotation, revocation, and real Vault microVM interoperability pass. | Operator: choose and operate the production secret/KMS service, workload identity, PKI, availability, and audit retention. The platform does not deploy Vault, KMS, or HSM infrastructure. |
| P10-03 telemetry export | Platform OTLP activation, correlation, bounded export path, and degraded behavior pass locally. | Operator: provide authenticated local collection, buffering or an accepted-loss policy, retention, capacity, and backend availability. The platform does not operate the telemetry backend. |
| P10-04 alert contract | Platform metrics and alert rules pass locally. | Operator: connect the real on-call receiver and prove delivery, ownership, inhibition, repetition, and Alertmanager availability. |
| P10-05 redaction/security | Platform header sanitization, redaction, authorization negatives, and application attribution pass locally. | Release/operator: rerun the signed candidate through the real ingress, telemetry, audit, and retention paths. OIDC application checks validate the fixture and integration, not a required platform service. |
| P10-05A API-gateway OIDC | Authentication, audience, scope/role, and negative cases pass on every node and through the front door. | Operator: validate the selected production IdP, HTTPS/PKI, rotation, outage behavior, and external routing. |
| P10-05B node-local mesh | Literal `.internal` DNS, `every_node` placement, local dependency failure, roles, and sustained concurrency pass. | Operator: preserve co-location and node-local routing on every production node class. Cross-host mesh identity is deliberately out of scope. |
| P10-06 Firecracker/kernel fixture | Disposable testbed image and eBPF-capable kernel pass. This is **not a platform production gate**. | Host operator: qualify the actual VPS/OS/kernel. Firecracker and KVM apply only if the production design chooses them. |
| P10-07 clock and PostgreSQL fixture | Platform clock degradation/recovery and application database integration pass locally. | Operator: provide production time sources. The OIDC Hub owns its PostgreSQL requirement; database HA, backup, KMS, and lifecycle are application/operator concerns, not platform components. |
| P10-08 resource policy | Per-instance and aggregate admission, disk/inode reserve metrics, rejection, and representative soak pass locally. | Operator: size each real node/volume class, run longer representative and N-1 load, and prove paging and recovery. |
| P10-09 TLS and NATS | Platform TLS/private-CA and NATS mTLS contracts, fail-closed admission, readiness degradation, and recovery pass locally. | Operator: provide production PKI, NATS accounts/subject policy/HA, certificate rotation, load balancing, and failure-domain evidence. NATS is required; PostgreSQL, Vault, HAProxy, and the selected PKI products are not shipped platform services. |
| P10-10 workload isolation | The declared single-trust-domain contract, mandatory eBPF, dedicated cgroup-v2 boundary, lifecycle, and local attribution pass. | Operator: repeat on the exact signed candidate and every host class. Stronger hostile-tenant isolation requires separate node boundaries. |

## Minimum path to a production decision

1. Preserve and review the verified candidate evidence, including all
   time-bounded dependency exceptions.
2. Deploy that same candidate digest to production-equivalent staging using the operator's
   host automation, PKI, NATS, secrets integration, telemetry path, alert
   receiver, DNS, load balancer, and resource limits.
3. Run only the applicable platform runbooks against every node class and fault
   domain. Run workload-specific checks, such as PostgreSQL restore and OIDC
   browser journeys, only for applications that actually require them.
4. If the deployment claims host-failure tolerance or cross-host availability,
   execute the two-physical-host plan. Otherwise record that those claims are not
   made and accept the single-host failure boundary explicitly.
5. Record approvers, time-bounded exceptions, rollback authority, and the exact
   evidence links in the platform production deployment checklist.
6. Issue a `GO` or `NO-GO` for that exact digest and environment. This local
   reconciliation is not itself a production approval.
7. If approved for GA, create the semantic-version tag from the reviewed source
   commit and repeat the signed workflow plus independent admission on the
   tagged bytes.

The two-host plan is therefore optional infrastructure qualification. It is not
a Firecracker requirement, a platform-source gate, or a prerequisite for
publishing a legitimate signed platform release.

## Evidence boundary

The OIDC Hub, PostgreSQL, Vault microVM, HAProxy front door, Prometheus stack,
and Firecracker guests are deliberately useful validation fixtures. They prove
the platform's protocols, routing, failure reporting, telemetry, and workload
behavior under realistic conditions. They do not make those products part of
the platform release or authorize repository automation to manage production
instances of them.

The historical 2026-08-29 environment was destroyed as recorded in
`FINAL_DECISION.md`. The later `.prod-validation-p10-08-state.json` environment
is a separate continuation fixture and must remain running until the user
explicitly authorizes its state-scoped teardown.

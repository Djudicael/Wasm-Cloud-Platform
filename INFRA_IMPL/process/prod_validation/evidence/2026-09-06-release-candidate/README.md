# Signed release candidate evidence

Decision date: `2026-09-06`

Result: **PASS — SIGNED NON-PROMOTABLE RELEASE CANDIDATE VERIFIED**

This evidence closes the platform-wide candidate-build portion of P10-01. It
does not create a semantic-version tag, publish a GA release, or authorize a
particular production deployment.

## Candidate identity

- Source commit: `b5054721c6785121a88619624d3b4f4c4dbb916a`
- Source ref: `refs/heads/update-project`
- Workflow run: [33999995526](https://github.com/Djudicael/Wasm-Cloud-Platform/actions/runs/33999995526)
- Workflow result: `success`
- Run interval: `2026-09-05T23:57:01Z` to `2026-09-06T00:17:09Z`
- GitHub artifact ID: `9979437172`
- Artifact name: `wasm-cloud-platform-candidate-b5054721c6785121a88619624d3b4f4c4dbb916a`
- Artifact expiry: `2026-12-04T23:57:02Z`
- Candidate manifest ref: `candidate-b5054721c6785121a88619624d3b4f4c4dbb916a`
- Promotion mode: `candidate`
- Promotable: `false`

The workflow passed exact clean-source identity, locked workspace and eBPF
resolution, frozen release builds, the closed artifact allowlist, blocking
dependency policy, SPDX 2.3 generation, deterministic packaging, per-subject
provenance and SBOM attestations, archive provenance, pre-upload keyless
verification, and immutable artifact upload.

## Independent admission result

The downloaded archive was verified outside the workflow against the exact
commit, candidate ref, and candidate mode. The archive provenance and the
provenance plus SPDX predicate for every subject in `SHA256SUMS` were then
verified with GitHub CLI against:

- repository `Djudicael/Wasm-Cloud-Platform`;
- signer workflow `Djudicael/Wasm-Cloud-Platform/.github/workflows/release.yml`;
- source digest `b5054721c6785121a88619624d3b4f4c4dbb916a`;
- source ref `refs/heads/update-project`.

All checks passed. The release archive is 25,891,868 bytes with SHA-256
`e30e1f756066a80bfc4c18952c62fcdeaf603dce0b4682b6c08c37cffc47be6b`.

## Dependency-security interpretation

The candidate uses manifest schema 3. Its manifest binds `.cargo/audit.toml`
with SHA-256
`ad1761dc55e2c23e88655283287ca9d2574668bfb925b848d1c3f3d6b3e8ce0c`
and records the exact seven configured exceptions. The policy scan reports zero
unexcepted vulnerabilities and no unexcepted warnings across 747 locked
dependencies.

This is not represented as an exception-free graph. The unfiltered review has
one advisory whose vulnerable parsing path is not exposed in the platform's
Pingora metrics path (`RUSTSEC-2024-0437`), one conditional `lru` unsoundness
whose panicking-key-drop prerequisite is not present (`RUSTSEC-2026-0253`), and
five unmaintained transitive crates. Their ownership, reachability, review
deadline, and removal conditions are in
[`DEPENDENCY_SECURITY_EXCEPTIONS.md`](../../../DEPENDENCY_SECURITY_EXCEPTIONS.md).

`RUSTSEC-2023-0071` is no longer accepted: OIDC/JWT verification now uses the
AWS-LC backend and `rsa 0.9.10` is absent from the locked workspace, including
test targets. Tests generate ephemeral RSA keys through AWS-LC; no private key
fixture is committed. Focused proxy and internal-gateway testing passed 143
tests.

## Superseded candidate

Runs `33996441190` and `33998226793` successfully proved earlier iterations of
the build/attestation machinery, but their candidates are superseded. The first
had an incomplete manifest description of the audit policy; the second removed
the RSA advisory but still committed a non-production test key. Do not deploy
or promote either earlier candidate.

## Decision effect

There is no remaining platform-wide `NO-GO` caused by two-physical-host
validation. The signed candidate is legitimate for production-equivalent
staging under the supported single-trust-domain contract. A particular
deployment remains conditional on its selected host classes, PKI, NATS,
secrets, telemetry, alerting, ingress, capacity, and application requirements.
Two-host validation is required only if that deployment claims physical-host
failure tolerance or cross-host availability.

A GA release still requires an approved semantic-version tag and the same
workflow/admission checks against tagged bytes. That is a promotion decision,
not an unresolved platform source defect.

# Platform redaction and OIDC security validation

This runbook closes the local part of production blocker P10-05. It deliberately
separates platform controls from application-protocol controls. PostgreSQL and
the OpenID Connect WASI Hub are representative workloads used to exercise the
platform; they are not platform services that every production installation
must deploy.

## Control ownership

| Owner | Required controls |
|---|---|
| Wasm Cloud Platform | Remove caller-supplied platform identity and trace headers; generate authoritative correlation data; prevent secrets and query values from entering proxy, ingress, telemetry, and audit records; preserve exact deployment attribution; reject invalid node-admin credentials. |
| OIDC application | Validate registered redirect URIs before using them in any success or error response; enforce authorization-code/session lifetime and replay rules; bind nonce to the authorization transaction and ID token; avoid logging complete request URIs, credentials, cookies, tokens, and request objects. |
| Relying party/operator | Generate and compare opaque `state`; verify nonce in the returned ID token; configure exact redirect URIs; retain production ingress, telemetry, audit, WAF/CDN, and identity-provider evidence. |

`state` is an opaque value generated and validated by the relying party. The
authorization server echoes it but cannot independently decide that an arbitrary
RP value has "expired." Likewise, a nonce is bound to an authorization transaction
and the ID token rather than being an independently expiring server credential.
The meaningful server-side expiry gates are authorization codes, login sessions,
pushed/request objects where used, refresh tokens, and the application's internal
social-login state. Production validation must also exercise RP state mismatch and
ID-token nonce mismatch in the real client.

## Local prerequisites

- Use the production-like three-node Firecracker topology, PostgreSQL service,
  OIDC backend/frontend deployment, HAProxy gateway, and observability stack.
- Keep the exact topology state in `.prod-validation-single-host-state.json`.
- Run from WSL2; never point this runner at production.
- Deploy an OIDC artifact containing the application security fixes before
  accepting the invalid-redirect assertion.

## Repeatable platform test

```bash
bash scripts/vm/validate-platform-redaction.sh \
  --state-file .prod-validation-single-host-state.json \
  --output-dir INFRA_IMPL/process/prod_validation/evidence/\
2026-08-30-single-host/P10-05-security
```

The runner generates a high-entropy sentinel and injects it as a bearer token,
cookie, caller-supplied application identity, caller-supplied trace identity,
malformed trace context, and OIDC-like `code`/`state` query values. It then:

- proves every node admin API rejects the invalid bearer credential;
- checks HAProxy, bounded serial-log tails, operational/audit exports, HTTP
  artifacts, and the two selected Tempo traces with the repository redaction
  scanner;
- verifies backend and frontend traces carry their own exact deployment identity
  without cross-application attribution;
- verifies an unregistered OIDC redirect URI resolves only to the local error
  endpoint; and
- records only the sentinel SHA-256, never its value, plus an artifact checksum
  manifest.

The evidence directory is mode `0700`. Do not add credential/configuration files
to it. Redaction of telemetry does not mean credentials may be stored in logs or
that configuration-at-rest security is optional.

## OIDC source gates

Run these in the OIDC repository with a Linux target directory:

```bash
export CARGO_TARGET_DIR=/tmp/openid-connect-wasi-target
cargo test -p integration-tests security_tests:: -- --test-threads=1
cargo test -p openid-connect-wasi middleware::logging::tests
cargo clippy -p oidc-oidc -p openid-connect-wasi -p integration-tests \
  --all-targets -- -D warnings
cargo build -p openid-connect-wasi --target wasm32-wasip2 --release
```

The suite must reject an unregistered redirect before that value is trusted for
an error response, reject replayed and expired authorization codes, reject an
expired login session under `prompt=none`, and keep query values out of request
logs. API-key timing tests must compare equivalent rejection paths; comparing an
authorized endpoint response with an unauthorized response measures protected
work and usage accounting, not constant-time secret verification.

The disposable `oidc-wasm-dev seed` command prints local test credentials. It is
allowed only inside this disposable rehearsal and must never be used by a
production migration/deployment pipeline. Production bootstrap must obtain
secrets through its approved secret system and must not print them.

## Production promotion evidence

A local pass does not close the production boundary. For each candidate release:

1. Roll every candidate node from the signed platform artifact that contains the
   header sanitizer; repeat the injection test against the real ingress.
2. Run the application security suite against the exact signed OIDC artifact.
3. Scan the full retention path: cloud/load-balancer and WAF/CDN logs, node and
   application logs, traces, audit streams, dead-letter queues, support exports,
   crash reports, and CI artifacts.
4. Prove log access control, encryption, retention/deletion, tenant separation,
   bounded cardinality, error-preserving sampling, and redaction before storage.
5. Exercise real relying-party state mismatch, ID-token nonce mismatch, clock
   skew, expired sessions/codes/tokens, replay, PKCE, and exact redirect matching.
6. Preserve redacted evidence, hashes, source and artifact identities, test
   timestamps, and independent reviewer approval.

Any sentinel occurrence, attacker-controlled redirect, accepted invalid admin
credential, missing exact deployment identity, cross-application attribution, or
unreviewed telemetry sink is a fail-closed production gate.

## 2026-08-30 local result

The three-node Firecracker test passed the platform runner: all injected secret
and identity values were absent from the captured telemetry, invalid admin bearer
credentials were rejected, frontend/backend traces retained separate exact
deployment identities, and the invalid redirect used `/oidc/error`. The OIDC fix
was deployed as `oidc/openid-connect-wasi:v3cfbbd7d2cdb`, and database readiness
remained healthy.

The platform source now explicitly strips untrusted `x-app-id`, `x-trace-id`,
`traceparent`, and `tracestate` before authoritative injection. That final source
change has unit/workspace validation but was not rolled into the already-running
schema-10 node images; signed candidate-node rollout and the production retention
scan therefore remain promotion requirements.

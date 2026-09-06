# P10-05 redaction and OIDC negative-security evidence

Result: **LOCAL PASS / PRODUCTION PIPELINE EVIDENCE PENDING**

On 2026-08-30 the production-like three-node Firecracker environment exercised
the OIDC backend and frontend through the state-recorded HAProxy and platform
proxies. The application security correction was deployed as
`oidc/openid-connect-wasi:v3cfbbd7d2cdb`; readiness reported `database=ok`.

Validated locally:

- a high-entropy value injected into authorization, cookie, application-ID,
  trace-ID, malformed trace context, and query fields did not occur in the
  25 captured artifacts spanning the HAProxy log, bounded node serial logs,
  operational/audit exports, responses, and selected Tempo traces;
- invalid bearer credentials were rejected by every node admin endpoint;
- backend and frontend traces retained their own exact deployment identity and
  did not cross attribution boundaries;
- an unregistered redirect URI returned the local `/oidc/error` location rather
  than the attacker-controlled origin;
- the focused OIDC security suite passed 26/26 tests, including replayed and
  expired authorization codes and expired sessions, while request middleware
  logs only the URI path; and
- the API-key timing test now compares two equivalent invalid-secret paths rather
  than measuring the extra work of a successful authorized endpoint.

The source-level platform sanitizer removes caller-controlled `x-app-id`,
`x-trace-id`, `traceparent`, and `tracestate`. Unit, Clippy, and workspace checks
cover that change. The live schema-10 nodes predate this final hardening helper,
although their existing authoritative header injection and telemetry passed the
live sentinel test. A signed candidate rollout containing the final sanitizer is
still required for production evidence.

`RESULT_SUMMARY.json` is machine-readable and contains only the sentinel hash.
`SHA256SUMS` covers the generated evidence. This directory intentionally excludes
credential files and configuration-at-rest secrets.

Known boundaries:

- the disposable development seeder prints local credentials and is forbidden
  in production;
- the test covers the selected local telemetry path, not production load
  balancers, WAF/CDN logs, durable retention, support exports, or CI artifacts;
- relying-party `state` mismatch and ID-token nonce mismatch must be tested by the
  real RP; they are not independent authorization-server expiry credentials; and
- production requires the exact signed platform/application releases, complete
  retention-path scanning, access/retention controls, and independent review.

Repeat with
[`PLATFORM_REDACTION_AND_OIDC_SECURITY_VALIDATION.md`](../../../../PLATFORM_REDACTION_AND_OIDC_SECURITY_VALIDATION.md).

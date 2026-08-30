# P10-05 API gateway OIDC evidence

Result: **PASS within the single-host microVM boundary** on 2026-08-30.

The PostgreSQL-backed OpenID Connect WASI Hub issued an `admin-ui` access token
with issuer `http://127.0.0.1:8088`. Three platform nodes fetched the matching
JWKS through the bridge-only HAProxy bind while continuing to validate the
public issuer. The immutable `hello-axum` deployment was
`validation/gateway-auth-hello:oidc-273e060c-a9b9`, with artifact SHA-256
`273e060c6b6db7349151078f4426cb6551da0e948356b91465dc5730b60017bf`.

All three direct-node paths returned 200 for public health, 401 without a token,
and 200 with the real token. Through HAProxy, missing and malformed tokens,
wrong audience, and expiry returned 401. A present `admin` scope returned 200;
the absent `gateway:admin` scope returned 403. OIDC readiness remained
`database=ok`, all three nodes stayed healthy, and the environment was left
running for the next authorized validation.

`RESULT_SUMMARY.json` contains only status results, artifact identity, and
non-secret claim metadata. `SHA256SUMS` protects that summary. No token,
password, cookie, private key, or authorization header is retained here.

Runbook: [API_GATEWAY_OIDC_MICROVM_VALIDATION.md](../../../../API_GATEWAY_OIDC_MICROVM_VALIDATION.md).

Production remains subject to signed-release, TLS/PKI, key-rotation, role-policy,
provider-failure, multi-host, and production identity-provider gates.

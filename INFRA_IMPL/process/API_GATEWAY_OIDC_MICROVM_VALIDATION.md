# API gateway OIDC microVM validation

## Purpose and boundary

This procedure validates the Wasm Cloud Platform API gateway, not the OpenID
Connect Hub as a production dependency. The Hub is a realistic WASI identity
provider and token issuer used as a test fixture. An application may use any
standards-compliant production OIDC provider whose issuer, audience, algorithms,
claims, and key-rotation behaviour have been qualified.

The local topology contains three platform-node microVMs, a NATS microVM, the
PostgreSQL-backed OIDC Hub, and host HAProxy. `hello-axum` is deployed with:

- public `GET /health`;
- authenticated `GET /`;
- authenticated `GET /app-health` requiring the `admin` scope; and
- authenticated `GET /requires-gateway-admin` requiring a scope the test token
  does not possess.

The final path does not need to exist in the application: the gateway must deny
it before dispatch when the scope is absent.

## Network and trust design

Browser-visible tokens carry issuer `http://127.0.0.1:8088` in this disposable
environment. A microVM cannot use that host loopback address. The node therefore
keeps the public issuer for strict `iss` validation while retrieving keys from
`http://172.20.0.1:8088/oidc/jwks` on the bridge. This is split-horizon OIDC:

```text
browser -> 127.0.0.1:8088 -> OIDC Hub
node    -> 172.20.0.1:8088 -> same Hub JWKS
token iss validation      -> 127.0.0.1:8088
```

Production must use HTTPS, a stable public issuer, a private JWKS/discovery
route when needed, trusted CA policy, DNS and egress controls, and tested key
rotation. The private endpoint changes key retrieval only; it never changes the
expected issuer or audience.

## Provisioning configuration

For a new environment, pass all three options together:

```bash
bash scripts/vm/provision-testbed.sh \
  --preset production-like \
  --nodes 3 \
  --front-door haproxy \
  --front-door-bind 127.0.0.1:8088 \
  --node-oidc-issuer-url http://127.0.0.1:8088 \
  --node-oidc-audience admin-ui \
  --node-oidc-jwks-url http://172.20.0.1:8088/oidc/jwks \
  --state-file .prod-validation-single-host-state.json
```

For an existing environment, rebuild node image schema 11 and roll nodes one at
a time. Supply the OIDC values on the first restart; persisted state applies
them to later restarts and scale-out nodes:

```bash
CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target \
  cargo build -p vm-testbed --bin vm-testbed-cli

sudo -E /tmp/wasm-cloud-platform-target/debug/vm-testbed-cli restart-node \
  --id local-test-node-0 \
  --state-file .prod-validation-single-host-state.json \
  --oidc-issuer-url http://127.0.0.1:8088 \
  --oidc-audience admin-ui \
  --oidc-jwks-url http://172.20.0.1:8088/oidc/jwks
```

Health-check each node before replacing another. Do not roll all nodes at once.

## Front door and application validation

Configure the OIDC and authenticated application host routes:

```bash
bash scripts/vm/configure-oidc-test-gateway.sh \
  --state-file .prod-validation-single-host-state.json \
  --platform-auth-host gateway-auth.internal
```

Run the repeatable validator:

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target
bash scripts/vm/validate-api-gateway-oidc.sh \
  --state-file .prod-validation-single-host-state.json \
  --evidence-dir INFRA_IMPL/process/prod_validation/evidence/DATE/P10-05-api-gateway-oidc
```

The validator builds `hello-axum` for `wasm32-wasip2`, deploys an immutable
digest-derived version, obtains a real Hub access token, creates signed expired
and wrong-audience variants with the disposable test key, and checks every node
plus the front door. Token values, passwords, and private keys are never copied
to evidence.

Required results:

| Check | Result |
|---|---:|
| Public health without a token | 200 |
| Protected route without a token | 401 |
| Protected route with a valid token | 200 |
| Malformed token | 401 |
| Correctly signed token with wrong audience | 401 |
| Correctly signed expired token | 401 |
| Present required scope | 200 |
| Missing required scope | 403 |

## Observations and production requirements

Two platform gaps were found and fixed during the rehearsal:

1. OIDC discovery previously coupled the expected public issuer to the network
   endpoint used by nodes. `OidcConfig.jwks_url` now permits a private key route
   without relaxing issuer validation.
2. `wasm-ctl deploy` previously assumed admin and artifact APIs shared one port.
   `--artifact-api` now supports the production-shaped `9090`/`9091` split.

Before production promotion, additionally prove:

- issuer/JWKS TLS validation, certificate rotation, key rotation and overlapping
  old/new keys;
- provider outage behaviour, bounded JWKS cache age, alerting, and the chosen
  fail-open/fail-closed policy (authentication should normally fail closed);
- role checks with both accepted and rejected realm/client roles;
- clock synchronization and bounded skew on every node;
- revocation/session policy, token lifetime, logout semantics, and refresh-token
  handling at the application boundary;
- rate limits for authentication failures without logging credentials or token
  query values;
- independent signed-release deployment and multi-host repetition.

Possession of the OIDC signing key is acceptable only in this disposable local
test. Production negative tokens must come from a dedicated test issuer or
controlled pre-production key, never from production signing-key access.


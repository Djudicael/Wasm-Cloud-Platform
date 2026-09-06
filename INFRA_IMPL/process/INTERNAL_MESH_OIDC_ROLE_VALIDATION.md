# Internal mesh OIDC role validation

## Purpose and boundary

This procedure validates the platform's node-local east-west gateway. The
OpenID Connect WASI Hub is a realistic token issuer used as a test fixture; it
is not a required production dependency of the platform.

The validation proves that a WASI caller resolves and reaches a role-protected
WASI target through its literal `<app>.<namespace>.internal:9080` URL, that the
gateway derives workload identity from the eBPF source-port/TID map under
sustained concurrency, and that it applies namespace and OIDC endpoint policy
before forwarding. It also proves co-location admission, fail-local dependency
behavior, and recovery after dependency redeployment.

## Tested policy

The target exposes:

| Endpoint | Policy | Expected claim |
|---|---|---|
| `/health` | public | none |
| `/echo` | realm role | `realm_access.roles` contains `mesh-admin` |
| `/info` | client role | `resource_access.mesh-api.roles` contains `operator` |

The script tests every case on each of the three platform nodes:

- missing bearer token and a forged `X-User-Roles` header return 401;
- a valid token with no role or the wrong realm/client role returns 403;
- the required realm and client roles return 200 and prove target content;
- a correctly authorized token from a workload in `mesh-attacker` still returns
  403 because namespace authorization runs independently of user roles.

## Run

Run in WSL with the existing production-like state and OIDC Hub available:

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target
bash scripts/vm/validate-internal-mesh-oidc-roles.sh \
  --state-file .prod-validation-single-host-state.json \
  --public-url http://127.0.0.1:8088 \
  --evidence-dir INFRA_IMPL/process/prod_validation/evidence/2026-08-30-single-host/P10-05-node-local-mesh-production
```

The script builds digest-identified `hello-axum` and `echo-service` WASI
artifacts, deploys a same-namespace caller and a cross-namespace caller, obtains
a real Hub token, creates controlled signed claim variants with the disposable
test key, and records only status results and artifact digests. Tokens, keys,
passwords, and response bodies are not retained in evidence.

## Implementation finding and remediation

The original image exposed the DNS stub on a nonstandard port that the guest
WASI resolver could not use. Node image schema 12 binds the node-local stub to
UDP port 53, places `nameserver 127.0.0.1` first, and returns `SERVFAIL` for
non-internal names so an operator resolver can answer them. The final fixture
uses the literal generated `.internal` URL; there is no loopback test override.

The second attempt reached the gateway but returned 401 before role evaluation.
The TCP state event and userspace ring-buffer consumer are asynchronous, so the
gateway could accept the request before the source-port/TID binding was visible.
The gateway now polls for at most 50 ms, at 1 ms intervals. It still fails closed
with 401 if no binding arrives, eBPF is inactive, or the map is unavailable.
Unit tests cover both delayed publication and permanent absence.

The first sustained run then exposed cumulative outbound connection accounting:
the WASI socket hook counted connects but could not observe closes. eBPF TCP
close events now release only outbound reservations for the mapped instance.
Production deployments that rely on `max_outbound_connections` must configure
eBPF as required, because a degraded userspace-only path cannot provide close
events.

The sustained run also exposed an unrelated runtime lifetime bug. A 30-second
epoch trap suitable for individual WASI HTTP request stores was also applied to
long-lived CLI-style Axum servers. Service stores now use continuing epoch
checkpoints and remain bounded by fuel, memory, health, and supervisor shutdown;
request stores retain the 30-second wall-clock ceiling. The validator waits 35
seconds before obtaining its short-lived token to prove the fix.

The final live run passed:

- exact caller application/TID resolution and 24 authorization checks on all
  three nodes;
- 96/96 calls per node at concurrency 8, with all nodes exercised at once;
- direct `.internal` DNS resolution on every node;
- declared `every_node` dependency placement;
- dependency removal returning 502 on all three nodes without remote fallback;
- dependency redeployment restoring 200 on all three nodes.

Machine-readable evidence is in
`prod_validation/evidence/2026-08-30-single-host/P10-05-node-local-mesh-production/`.

## Production gates

Before promoting a signed production node image, also prove:

- `.internal` DNS resolution from that exact signed image on every production
  node and verify the configured secondary resolver;
- a maintained production kernel and mandatory eBPF availability/alerting;
- workload identity behavior during ring pressure, consumer restart, and node
  rolling replacement;
- production IdP HTTPS/PKI, accepted algorithms, claim mapping, key rotation,
  issuer failure, clock skew, and token revocation/session policy;
- explicit cross-namespace allow rules, their audit records, and negative tests;
- every dependent application declares `placement.policy = "every_node"` and
  fully qualified same-namespace `local_dependencies`;
- dependency failure returns the documented 502/degraded behavior and never
  searches another node;
- latency and capacity impact of the bounded identity wait under expected load.

The role-bearing user token proves endpoint authorization. It is not by itself a
workload credential or mutual-authentication mechanism. Cross-host mesh identity
is explicitly out of scope by architecture, not an incomplete platform feature.
Traffic that intentionally leaves a node must use an explicit external endpoint
and be validated under that endpoint's TLS, identity, and authorization policy.

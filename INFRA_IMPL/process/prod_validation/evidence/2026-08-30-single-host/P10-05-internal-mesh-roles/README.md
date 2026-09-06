# P10-05 internal mesh and OIDC role evidence

> Historical result, superseded by
> `../P10-05-node-local-mesh-production/README.md`. This earlier run used a
> loopback test override and did not validate the final DNS, placement,
> dependency-failure, service-lifetime, or sustained-concurrency contract.

Result: **PASS within the single-host, node-local microVM boundary**

On 2026-08-30, the same digest-identified target and callers were deployed to
three Firecracker platform nodes. All 24 expected authorization outcomes passed:

- realm role positive and negative cases;
- client-specific role positive and negative cases;
- missing-token and forged-role-header rejection; and
- cross-namespace denial even with a role-authorized token.

Node logs independently recorded the exact same-namespace and cross-namespace
caller application IDs, source ports, and TIDs. Each cross-namespace identity
was resolved and then denied; the result was not caused by an unresolved caller.

`RESULT_SUMMARY.json` contains the status matrix and artifact SHA-256 values.
`SHA256SUMS` authenticates the retained result file. No token, signing key,
password, request body, or response body is stored here.

This historical run did not prove production DNS, production IdP/PKI, or
provider failure and key rotation. Cross-host mesh traffic and identity are
explicitly out of scope by architecture, not a remaining platform feature. See
`INFRA_IMPL/process/INTERNAL_MESH_OIDC_ROLE_VALIDATION.md` for the procedure,
observations, and remaining gates.

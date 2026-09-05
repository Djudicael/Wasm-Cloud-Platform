# P10-09 platform TLS and NATS integration evidence

Date: 2026-09-05  
Scope: platform-owned TLS listeners and NATS client behavior on WSL2/Linux.

## Result

The local platform contract passed. A real `wasm-node` connected to a disposable
NATS 2.10 service through a private CA and mutual TLS. HTTPS worked on the
Pingora proxy and the built-in admin, deploy-ingress, and artifact listeners;
plaintext was rejected. Stopping NATS changed readiness to HTTP 503 with the
NATS dependency unhealthy, and restarting it restored HTTP 200 without a node
restart. Missing certificate material failed node startup.

The first runtime attempts exposed two platform defects that unit-only checks
did not reveal:

- Rustls had two possible cryptographic backends in the final process and no
  explicit default provider, causing startup to panic when TLS was activated.
  The node now installs the AWS-LC provider before parsing configuration.
- Cleartext h2c and TLS shared one Pingora service. TLS connections could not be
  reliably peeked for the h2c preface and were treated as HTTP/2, breaking normal
  HTTPS handshakes. Cleartext h2c and TLS/ALPN now use separate services.

The production template also formerly specified plaintext NATS despite the
production validator requiring `tls://`, and the built-in artifact listener
remained plaintext when admin TLS was configured. Both contracts were corrected.

The final dependency gate then found RustSec advisories RUSTSEC-2026-0268 and
RUSTSEC-2026-0269 in Wasmtime 47.0.3. The workspace and complete Wasmtime family
were updated to 47.0.4. `cargo audit --deny warnings` passed afterward, and the
64-test runtime suite passed, including `test_list_hello_axum_exports`.

## Boundary and limitations

The validator deliberately created a disposable local NATS container beside the
currently running microVM rehearsal and did not mutate that recorded topology.
It proves platform protocol support, fail-closed startup, readiness behavior,
and reconnect. It does not prove production PKI operation, NATS quorum,
multi-host failure domains, managed-load-balancer behavior, service backup, or
certificate rotation. Those are operator/provider evidence described in
`PLATFORM_TLS_AND_NATS_PRODUCTION_VALIDATION.md`.

PostgreSQL, Vault/KMS/HSM service HA, immutable retention, and off-host backup
systems are not components of the Wasm platform and are not claimed by this
result.

## Evidence files

- `RESULT_SUMMARY.json`: redacted initial, outage, and recovered health plus the
  asserted TLS contract.
- `SHA256SUMS`: integrity hashes for the evidence package.

The private CA, leaf certificates, private keys, temporary NATS storage, and
node database were destroyed by the script's exact-target cleanup.

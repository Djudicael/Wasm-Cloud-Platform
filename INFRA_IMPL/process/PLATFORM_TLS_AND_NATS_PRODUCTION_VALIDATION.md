# Platform TLS and NATS production validation

## Purpose and ownership boundary

This runbook closes the platform-owned portion of P10-09. It validates that the
Wasm Cloud Platform can authenticate a private NATS PKI, present HTTPS on every
built-in network listener, reject plaintext and invalid TLS material, report a
NATS outage through readiness, and reconnect after service recovery.

The platform does not deploy or operate a production certificate authority,
NATS cluster, load balancer, Vault/KMS/HSM, PostgreSQL cluster, backup system, or
retention service. Operators must qualify those services separately. PostgreSQL
is not a platform dependency; it is included only when an application needs it.

| Responsibility | Platform evidence | Operator evidence |
|---|---|---|
| Node proxy, admin, deploy-ingress, and artifact TLS | Listener starts, certificate parses before readiness, HTTPS works, plaintext fails | Issuance, SANs, renewal, revocation, expiry alerting, private-key delivery |
| NATS client security | `tls://`, private CA, optional credentials and mTLS, outage/readiness and reconnect | HA topology, accounts/subjects, quorum, storage, backups, certificate lifecycle |
| External load balancer | Correct node HTTPS/readiness behavior | TLS termination model, health checks, draining, WAF, failover, provider behavior |
| Secret and key services | Supported client integrations and fail-closed behavior | Service HA, recovery, KMS/HSM controls, immutable audit retention |
| Application database | No platform claim | Application/operator HA, backup, restore, encryption, retention |

## Configuration contract

Production node configuration must use `tls://` for NATS and must provide a
credentials file. A private CA and mutual-TLS identity are explicit fields:

```toml
[nats]
url = "tls://nats.prod:4222"
creds_file = "/run/credentials/wasm-node/nats.creds"
ca_cert = "/etc/wasm-node/nats/ca.crt"
client_cert = "/run/credentials/wasm-node/nats-client.crt"
client_key = "/run/credentials/wasm-node/nats-client.key"
```

`client_cert` and `client_key` are an indivisible pair. The equivalent node
environment variables are `WASM_NODE_NATS_CA_CERT`,
`WASM_NODE_NATS_CLIENT_CERT`, and `WASM_NODE_NATS_CLIENT_KEY`. `wasm-ctl` uses
the `WASM_CTL_NATS_*` prefix; standalone deploy ingress uses
`WASM_DEPLOY_INGRESS_NATS_*`.

The admin certificate is used by the built-in admin, deploy-ingress, and
artifact listeners. When it is absent they fall back to the proxy certificate.
Production artifact advertisement must be an explicit `https://` URL. The
certificate SAN must cover every hostname used by callers.

The cleartext proxy listener supports h2c. The TLS proxy listener is a separate
Pingora service and negotiates HTTP/2 or HTTP/1.1 with ALPN. Do not enable h2c
sniffing on the TLS service: a TLS stream cannot reliably expose the cleartext
HTTP/2 preface to Pingora's peek logic.

## Repeatable local platform contract

Run in Linux or WSL2. The script creates a one-day private test CA, a disposable
mTLS NATS container, and one native candidate node. It does not alter or destroy
any recorded Firecracker testbed.

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target

bash scripts/vm/validate-platform-tls-contract.sh \
  --evidence-dir /tmp/p10-09-platform-integration
```

The test must prove all of the following:

- NATS connects through a private CA and requires a client certificate;
- proxy, admin, deploy-ingress, and artifact listeners accept HTTPS;
- plaintext requests do not succeed on those TLS ports;
- unreadable or malformed listener material prevents node startup;
- stopping NATS makes node readiness return HTTP 503 with NATS unhealthy;
- restarting the same NATS service restores readiness without restarting the node;
- cleanup targets only the exact generated node PID, container name, and temporary directory.

This is protocol and failure-contract evidence, not NATS HA or production PKI
evidence. A separate local NATS process is deliberate: the existing Firecracker
testbed uses a single plaintext fixture and must not be silently redefined as a
production service design.

## Staging and production acceptance

Before promotion, repeat the checks with the exact signed node artifact and the
actual staging integrations:

1. Issue node and NATS identities from the production-equivalent PKI. Verify SAN,
   trust-chain, expiry, file ownership, and least-privilege identity mapping.
2. Connect every platform node to all advertised NATS cluster endpoints using
   its real account/subject authorization. Prove unauthorized subjects fail.
3. Rotate server CA/intermediate, server leaf, client leaf, NATS credentials,
   proxy leaf, and admin/artifact leaf without exceeding the availability SLO.
4. Remove or revoke each old identity and prove it can no longer connect.
5. Fail one NATS member, the stream leader, one failure domain, and the client
   network path. Record quorum, publish/consume continuity, readiness, alerts,
   bounded retries, and recovery.
6. Exercise the real load balancer through HTTP/1.1 and HTTP/2. Verify health
   checks, client-IP trust, TLS termination/re-encryption, draining, idle
   timeouts, maximum request sizes, and rollback.
7. Confirm admin and artifact ports are private and authenticated; only approved
   ingress paths can reach them.
8. Preserve redacted configurations, certificate metadata (never private keys),
   command versions, health results, alerts, rotation/failure timelines, signed
   artifact digests, and an independent review.

P10-09 is production-closed only when the local platform contract passes and
the operator supplies this deployment-specific evidence. Passing the local
script alone is not production approval.

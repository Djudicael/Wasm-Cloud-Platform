# Vault Transit microVM validation

This runbook validates the platform seal root against a real HashiCorp Vault
server running in a Firecracker service microVM. It is the local acceptance
gate for the Vault-specific portion of P10-02. It does not turn the disposable
test Vault into a production secret-manager design.

## Topology and security model

Vault is a service microVM, not a platform node. The validated topology has
three platform nodes, one NATS microVM, PostgreSQL, Vault, host HAProxy, and the
disposable observability stack. Vault defaults to `172.20.0.21:8200` and is
recorded in the same topology state as the other Firecracker processes.

The local image uses HashiCorp Vault Community 1.21.4 from the official release
site with a pinned architecture-specific SHA-256, regular sealed file storage
(not dev mode), TLS with a short-lived private CA, a non-exportable 256-bit
Transit HMAC key, separate node/operator policies, and CIDR-bound AppRoles.
SecretIDs use single-use 60-second response wrapping and issue short-lived batch
tokens.

This is an external-integration fixture. The platform neither requires
HashiCorp Vault specifically nor owns Vault's production deployment. A
production operator may use the other admitted external HMAC seal-root
implementation. This drill proves the platform's Vault client, admission,
rotation, outage, and recovery behavior; it does not qualify Vault HA.

The image bootstrap contains a one-share Shamir unseal key and initial root
token. It is a disposable test fixture and must remain on a Linux filesystem
with mode 0600. It is never written under the Windows-mounted repository.
Production requires approved HA storage and seal design, workload identity,
quorum/recovery ceremony, audited break-glass access, and restore procedures.

## Build and provision

Run in WSL from the repository root:

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target

bash scripts/vm/build-vault-rootfs.sh
bash scripts/vm/provision-vault-service.sh \
  --state-file .prod-validation-single-host-state.json
```

The local image and CA are created under `assets/`. The initialized image
contains Vault file storage and its local TLS private key, so it must not be
published or promoted. The protected bootstrap is
created at:

```text
${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-vault-build-$(id -u)/bootstrap.json
```

The provisioner uses the canonical testbed CLI, records `vault-secrets`,
unseals it, exchanges response-wrapped AppRole SecretIDs, and stores the
resulting credentials in a state-keyed mode-0700 runtime directory. The
companion `.services.json` contains only paths and metadata, never credentials.

Verify without printing secrets:

```bash
jq '.services[] | select(.id == "vault-secrets")' \
  .prod-validation-single-host-state.json

curl --fail --silent --show-error \
  --cacert assets/vault-test-ca.crt \
  https://172.20.0.21:8200/v1/sys/health \
  | jq '{initialized,sealed,standby,version}'
```

Expected: initialized, unsealed, active, version 1.21.4.

## Acceptance drill

```bash
bash scripts/vm/validate-vault-transit-microvm.sh \
  --state-file .prod-validation-single-host-state.json
```

The runner refreshes AppRole credentials and validates:

1. private-CA TLS verification;
2. unauthenticated HMAC rejection;
3. HMAC permission but rotation denial for the node role;
4. rotation permission for the operator role;
5. distinct deterministic HMAC outputs for pinned old/new versions;
6. actual `wasm-node` initialization against real Vault;
7. rewrap of the persisted KEK and transport private key;
8. restart with only the new version;
9. fail-closed startup while Vault is sealed;
10. successful startup after unseal;
11. temporary socket-audit capture and preserved request IDs;
12. sentinel scanning of node logs and Vault audit records.

The deterministic node milestone is completion of the sealed transport-key
load path while the process remains alive. HTTP health also counts if it arrives
first. This avoids treating retained-event replay time from a busy NATS stream
as a Vault interoperability failure.

Evidence is written under:

```text
INFRA_IMPL/process/prod_validation/evidence/2026-08-30-single-host/P10-02-vault-microvm/
```

Verify it with:

```bash
sha256sum -c \
  INFRA_IMPL/process/prod_validation/evidence/2026-08-30-single-host/P10-02-vault-microvm/SHA256SUMS
```

## Validated result (2026-08-30)

The drill passed. The final run rotated Transit version 3 to 4. The node
initialized sealed state, rewrapped both persisted envelopes, restarted without
the previous version, refused startup while Vault was sealed, and recovered
after unseal. Least-privilege checks, audit capture, request-ID correlation,
checksums, and the sentinel scan passed. Vault and the platform remain running.

## Problems found and fixed

Real Vault exposed issues that mocks did not prove:

- Private Vault PKI was unusable because the node trusted only bundled public
  roots. `runtime.key_vault_ca_cert` and
  `WASM_NODE_RUNTIME_KEY_VAULT_CA_CERT` now load a PEM CA bundle.
- Real HMAC responses use `vault:vN:<base64>`; the mock used hex. The decoder
  now accepts real base64, retains legacy hex compatibility, and requires 32
  decoded bytes.
- Vault HMAC keys reject `derived=true`. The node context is HMAC input for
  domain separation, not Vault's derived-key feature. The compatible command is
  `vault write transit/keys/wasm-platform-seal type=hmac key_size=32
  exportable=false allow_plaintext_backup=false`.
- Vault required an explicit 32-byte HMAC key size.
- Alpine `/sbin/init` is a symlink. Writing through it launched OpenRC, so the
  builder now removes it before installing deterministic PID 1.
- Vault 1.21 rejects non-TTY CLI unseal input. Automation now uses the HTTPS
  unseal API, keeping the key out of process arguments.
- WSL Windows-mounted files report permissive modes. Bootstrap and AppRole
  material therefore stays under `XDG_RUNTIME_DIR` (or `/tmp`), not `/mnt`.
- HTTP readiness was delayed by retained NATS replay even after seal setup had
  succeeded. The focused drill now waits on the relevant cryptographic state.

## Remaining production evidence

This proves the platform's local Vault protocol compatibility and failure
behavior. For a production deployment that selects Vault, the infrastructure
operator still qualifies Vault storage availability, production PKI
renewal/revocation, workload identity and renewal, approved auto-unseal or
multi-person recovery, durable off-host audit retention, and restore. Those are
external-service deployment gates, not missing Wasm platform implementation.
The platform candidate still needs rolling rotation on its actual nodes,
all-node application-secret acknowledgements, admin new-token success/old-token
rejection, and platform-state restore evidence.

Do not destroy the environment after this drill unless the user explicitly asks.
The canonical teardown removes the recorded Vault VM and its exact state-scoped
credential directory.

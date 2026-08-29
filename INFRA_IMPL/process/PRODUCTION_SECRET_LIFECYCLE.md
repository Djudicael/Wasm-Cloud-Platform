# Production secret lifecycle

This is the operator gate for P10-02. It applies to every platform node and to
the standalone deploy ingress. Local microVM credentials and the OIDC seeder are
test fixtures; they are never acceptable production inputs.

The platform consumes an external non-exportable HMAC seal root; it does not
deploy the external key manager. Vault Transit and AWS KMS HMAC are supported
implementations, not cumulative requirements. The platform release gate proves
admission, interoperability, rewrap, fail-closed behavior, and recovery. If a
deployment selects Vault, KMS, or an HSM-backed service, that service's HA,
backup, PKI, identity, and operational lifecycle remain the infrastructure
operator's separate responsibility.

## Enforced production policy

Set `node.environment = "production"` (or
`WASM_NODE_ENVIRONMENT=production`) on every node. Startup and SIGHUP reload
then fail closed unless all of these are true:

- admin authentication is enabled with different 32-byte random read/write
  tokens encoded as 64 hexadecimal characters;
- admin TLS material is configured and the legacy `admin.auth_token` is absent;
- NATS uses `tls://` and a credentials file;
- the node seal key is derived from a non-exportable Vault Transit HMAC key or
  AWS KMS HMAC key;
- Vault and KMS endpoint overrides use HTTPS;
- Vault Transit uses an explicitly pinned key version;
- an inline DNS webhook token is absent.

The node rejects a legacy plaintext auth override in redb in production. The
`/admin/auth/rotate-token` endpoint is also disabled there: production token
rotation belongs to the external manager and controlled config reload, not to
node-local persistence.

Stop and drain the node first. Before the first production-mode start of an upgraded node that has a legacy
override, rotate both tokens in the external manager and remove only that exact
redb record with:

```bash
wasm-node --db-path /var/lib/wasm-node/state.redb \
  --clear-persisted-auth-override
```

Back up the state file first. This one-shot command does not start listeners or
connect to NATS and does not delete application secrets.

Example Vault configuration (values and paths are illustrative):

```toml
[node]
node_id = "node-1"
environment = "production"

[nats]
url = "tls://nats.service.internal:4222"
creds_file = "/run/secrets/nats.creds"

[admin]
tls_cert = "/run/pki/admin.crt"
tls_key = "/run/pki/admin.key"

[auth]
enabled = true
read_token = "<64-hex value projected by the secret manager>"
write_token = "<different 64-hex value>"
require_tls = true

[runtime]
key_source = "vault-transit"
key_vault_url = "https://vault.service.internal:8200"
key_vault_token_env = "VAULT_TOKEN"
key_vault_ca_cert = "/run/pki/vault-ca-bundle.pem"
key_vault_transit_mount = "transit"
key_vault_transit_key = "wasm-platform-seal"
key_vault_transit_context = "cluster-a/node-1"
key_vault_transit_key_version = 7
```

Use a short-lived Vault workload identity/token restricted to the single HMAC
operation and key. With AWS, grant only `kms:GenerateMac` on the selected HMAC
key and configure `key_aws_kms_region`, `key_aws_kms_key_id`, and a unique,
stable `key_aws_kms_context`. Never share derivation context between nodes.

`key_vault_transit_context` is base64-encoded and sent as the Transit HMAC
input. This provides deterministic per-node domain separation; it is not
Vault's `derived=true` key feature. Create a compatible non-exportable key with:

```bash
vault write transit/keys/wasm-platform-seal \
  type=hmac key_size=32 exportable=false allow_plaintext_backup=false
```

For private Vault PKI, configure `runtime.key_vault_ca_cert` (or
`WASM_NODE_RUNTIME_KEY_VAULT_CA_CERT`) with the readable PEM CA bundle. The
node verifies the certificate hostname/SAN and has no trust-all fallback.

Standalone deploy ingress uses
`WASM_DEPLOY_INGRESS_ENVIRONMENT=production`. It must bind to loopback behind
the TLS front door, use strong separate auth tokens, TLS NATS credentials, HA,
signature allow-lists, and digest-pinned OCI references. Its derived envelope
key must be projected at `--key-file` by the external secret agent into a
read-only tmpfs with mode 0600 or stricter; it must not live in an image,
repository, persistent volume, command line, or ordinary environment variable.

## Application secret rotation and revocation

`wasm-ctl secrets set` encrypts a separate value for every active node. After a
node persists a rotation, it evicts that application's warm instances. The next
request cold-starts with the new value; therefore a successful command means the
old value is no longer usable by an indefinitely warm instance.

```bash
read -rsp 'New value: ' ROTATED_VALUE; echo
printf '%s' "$ROTATED_VALUE" | wasm-ctl secrets set \
  --app namespace/application:v2 --key DATABASE_PASSWORD --value-file -
unset ROTATED_VALUE
```

Omit `--value` in normal use so the value does not enter shell history. For
automation, provide it through protected standard input. Verify application
readiness and a real authenticated transaction, then revoke the old credential
at its source.

```bash
wasm-ctl secrets delete \
  --app namespace/application:v2 --key DATABASE_PASSWORD
```

Delete events target every node in the authoritative registry, including stale
nodes, so a reconnecting node consumes the revocation. A receiving node deletes
the local value and evicts warm instances. New cluster nodes cannot receive a
deleted value through bootstrap because it is no longer in the provider.

Do not remove a retired node from the authoritative registry until its durable
secret-delete events are consumed or its local state is securely destroyed.
Secret deletion is protocol version 2; complete that platform protocol rollout
before issuing delete events. Freeze secret mutations during mixed-version
platform upgrades.

## External seal-key rotation

Vault Transit key rotation changes HMAC output. An unpinned version would make
the persisted KEK and node transport key unreadable, so production admission
requires a pinned version.

For a controlled Vault rotation:

1. Record backups, current version `N`, node readiness, and secret smoke tests.
2. Rotate the Transit key to `N+1` but retain version `N` for HMAC operations.
3. Set `key_vault_transit_key_version = N+1` and
   `key_vault_transit_previous_key_version = N` on one node.
4. Restart that node. Startup first tries `N+1`, falls back to `N`, and
   atomically re-encrypts both its persisted KEK and secret-transport private
   key under `N+1`.
5. Restart it once more with the previous-version setting removed. This proves
   the rewrap is durable. Run application secret/readiness checks.
6. Repeat one node at a time. Only after every node passes may version `N` be
   retired according to the external-manager retention policy.

AWS KMS HMAC replacement uses the same sequence with
`key_aws_kms_key_id = <new>` and
`key_aws_kms_previous_key_id = <old>`. Do not schedule deletion of the old KMS
key until every node has restarted successfully without the previous-key field
and backups have passed restore validation.

## Admin token rotation

Update the secret-manager projection atomically, send SIGHUP, and verify both
that the new token works and the old token receives 401. The SIGHUP handler runs
the complete configuration admission policy before swapping tokens. Roll one
node at a time so operators retain access. If credentials are injected only at
process start, restart instead of SIGHUP.

## Redaction and evidence gate

The following are intentionally redacted in `Debug`: admin auth configs, token
rotation requests, and artifact credential requests. NATS connection logs no
longer include URLs because URLs may contain user information. Key-command
stderr is suppressed. Token rotation logs contain only the token class, never a
prefix. The OIDC rehearsal keeps its local password in a mode-0600 runtime file
and redacts credential-bearing seeder output instead of printing the password.

Before promotion, inject a unique sentinel into every secret path, execute the
rotation/revocation and one failed request, collect node, ingress, HAProxy,
application, audit, Collector, trace, and CI artifacts, then run:

```bash
WASM_SECRET_REDACTION_SENTINEL='<unique sentinel of at least 16 chars>' \
  scripts/validate-secret-redaction.sh path/to/logs/* path/to/reports/*
```

Preserve without secret values:

- external-manager audit IDs and key versions;
- config admission output;
- new-token success and old-token 401 status;
- application-secret rotation success and delete-event acknowledgements from
  every registered node;
- pre/post readiness and functional transaction results;
- seal-key rewrap restart results without the previous key;
- redaction scan result and hashes of the scanned artifacts.

Local unit tests and source review prove the fail-closed paths. A real Vault or
AWS workload-identity drill and log scan in staging are still release-specific
evidence and must be attached to the candidate before P10-02 is marked fully
closed for production promotion.

The repeatable implementation and 2026-08-30 passing local evidence are in the
[Vault Transit microVM validation](./VAULT_TRANSIT_MICROVM_VALIDATION.md). That
closes local Vault protocol compatibility; production workload identity, HA
storage, PKI, durable audit retention, and rollout evidence remain
operator/candidate-specific.

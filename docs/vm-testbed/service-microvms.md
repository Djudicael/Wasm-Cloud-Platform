# Local service microVMs

The Firecracker testbed can attach service microVMs to an existing platform
topology. These VMs validate integration paths; they are not platform nodes and
they are not production infrastructure managed by the Wasm Cloud Platform.

## Scope boundary

The local topology separates three concerns:

| Concern | Local implementation | Production ownership |
|---|---|---|
| Platform runtime | `wasm-node` microVMs and their embedded reverse proxies | Wasm Cloud Platform operators |
| Platform messaging dependency | One NATS/JetStream microVM | Operators must provide a production NATS design, normally highly available |
| Application dependency | Optional PostgreSQL microVM | The application or database operator; PostgreSQL is not required by the platform |
| External seal-root integration | Optional Vault Transit microVM | The selected external key-management operator; Vault is one supported integration, not a bundled platform service |
| Telemetry backends | Optional host Podman stack | Observability operators or managed services |

Consequently, the platform release pipeline must test its clients, admission
rules, degraded behavior, and recovery against representative external
services. It must not claim to deploy or qualify PostgreSQL, Vault high
availability, a KMS/HSM, or the telemetry backends themselves. Those services
have separate production architecture, availability, backup, PKI, and upgrade
gates.

## Images built by the aggregate command

Run all image builders in Linux or WSL2:

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target
bash scripts/vm/build-all-images.sh
```

The command creates or validates these artifacts:

- `assets/vmlinux-6.1`
- `assets/nats-rootfs.ext4`
- `assets/wasm-node-rootfs.ext4`
- `assets/postgres-rootfs.ext4`
- `assets/vault-rootfs.ext4`
- `assets/vault-test-ca.crt`

The Vault image is a disposable local fixture. It contains initialized Vault
file storage and a local TLS private key. Do not publish it, promote it as a
release artifact, copy it to production, or treat it as an empty reusable base
image. Its matching unseal/root bootstrap is written with mode 0600 outside the
Windows-mounted repository by default:

```text
${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-vault-build-$(id -u)/bootstrap.json
```

The CA certificate is not secret, but it trusts only this disposable fixture.
Production trust roots must come from the production PKI process.

## Attach application PostgreSQL

Only add PostgreSQL when the application under test needs it:

```bash
bash scripts/vm/build-postgres-rootfs.sh
bash scripts/vm/provision-postgres-service.sh \
  --state-file .prod-validation-single-host-state.json
```

The service is normally reachable at `172.20.0.20:5432`. Its VM lifecycle is
recorded in the selected topology state. Application migrations and database
health checks belong to the application rehearsal, not to platform startup.

## Attach and validate Vault Transit

Use Vault only when testing the platform's external seal-root integration:

```bash
bash scripts/vm/build-vault-rootfs.sh
bash scripts/vm/provision-vault-service.sh \
  --state-file .prod-validation-single-host-state.json
bash scripts/vm/validate-vault-transit-microvm.sh \
  --state-file .prod-validation-single-host-state.json
```

The Vault service is normally reachable over private-CA TLS at
`https://172.20.0.21:8200`. Provisioning unseals the existing fixture and
exchanges response-wrapped, single-use AppRole SecretIDs for short-lived node
and operator tokens. Tokens, the root token, and the unseal key are never
stored in the repository state files or printed.

The provisioner is intentionally idempotent for a recorded service. A rerun
refreshes short-lived AppRole credentials and preserves the state-scoped
bootstrap already associated with the running VM. Do not replace or rebuild a
Vault image while its Firecracker process is using it. If a new fixture is
required, destroy the recorded topology first and build a fresh image and
bootstrap together.

The full security model, acceptance checks, evidence, and problems found are in
[Vault Transit microVM validation](../../INFRA_IMPL/process/VAULT_TRANSIT_MICROVM_VALIDATION.md).

## Attach the disposable telemetry stack

Persist the node OTLP endpoint when creating the topology, then provision the
state-scoped host stack:

```bash
bash scripts/vm/provision-testbed.sh \
  --preset production-like \
  --nodes 3 \
  --node-otlp-endpoint http://172.20.0.1:4317 \
  --state-file .prod-validation-single-host-state.json

bash scripts/vm/provision-observability.sh \
  --state-file .prod-validation-single-host-state.json
```

The companion service state records the exact Collector, Tempo, Prometheus,
Alertmanager, local alert-receiver, and exporter container identities plus the runtime directory and
separated operational/audit log paths. The Collector receives node OTLP over
the private test bridge, tails the exact recorded node serial logs, and uses a
bounded disk-backed queue when the trace backend is unavailable. Local export
files are created with mode 0600.

Run `scripts/vm/validate-alerting.sh --state-file <state>` to check all tracked
expressions against live metrics and exercise the local receiver. Its protected
notification JSONL path is recorded in companion state. This receiver exists
only to prove delivery, resolution, and deduplication; replace it with the
operator's authenticated on-call destination in production.

This fixture validates the platform integration; it is not a production
telemetry deployment. A production host needs a supervised local agent,
authenticated TLS, monitored queue storage, and independently operated
off-host trace, operational-log, and immutable audit destinations. The current
node exporter has no WAL, so spans created while its immediate Collector is
stopped are not guaranteed to survive. Follow the complete
[production telemetry validation](../../INFRA_IMPL/process/PRODUCTION_TELEMETRY_VALIDATION.md).

## State and credential locations

Keep the same `--state-file` value throughout provisioning, validation, and
teardown.

| Location | Contents | Secret material |
|---|---|---|
| `<state-file>` | Bridge, NATS, platform-node and service-VM identities/PIDs | No Vault tokens or unseal material |
| `<state-file>.services.json` | HAProxy, observability, and Vault lifecycle metadata and protected-file paths | Paths only |
| Recorded observability runtime directory | Collector queue, filelog offsets, generated configs, and mode-0600 operational/audit exports | Redacted telemetry only; still treat as protected evidence |
| `${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-vault-$(id -u)/<state-hash>/` | State-scoped bootstrap, CA, curl config, and short-lived tokens | Yes; directory 0700 and secret files 0600 |
| `${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-vault-build-$(id -u)/bootstrap.json` | Build-time unseal key, initial root token, role IDs | Yes; mode 0600 |
| `assets/vault-rootfs.ext4` | Initialized disposable Vault storage and TLS key | Sensitive local fixture |

Do not move protected runtime material under `/mnt/c`, `/mnt/d`, or another
Windows mount: WSL permission reporting there does not provide the intended
Linux file-mode protection.

## Status checks

Inspect lifecycle metadata without reading credential files:

```bash
jq '.services[] | {id,kind,ip,port,pid}' \
  .prod-validation-single-host-state.json
jq '{vault,observability,front_door}' \
  .prod-validation-single-host-state.json.services.json
```

Check Vault health with certificate validation:

```bash
curl --fail --silent --show-error \
  --cacert assets/vault-test-ca.crt \
  https://172.20.0.21:8200/v1/sys/health \
  | jq '{initialized,sealed,standby,version}'
```

Do not print, upload, archive, or add to evidence the bootstrap JSON, token
files, authenticated curl config, environment variables containing tokens, or
raw application secrets.

## Teardown and retained build artifacts

Only destroy an interactive environment after the user explicitly requests it:

```bash
bash scripts/vm/destroy-testbed.sh \
  --state-file .prod-validation-single-host-state.json
```

The canonical teardown validates exact recorded identities, stops the recorded
HAProxy/observability services and Firecracker VMs, removes the topology state,
and removes the exact state-scoped Vault and OIDC runtime credential
directories. It does not remove the reusable kernel/rootfs assets or the
build-time Vault bootstrap. Those retained local fixtures must be protected and
must never be committed or promoted. Delete or rebuild them deliberately when
the local fixture is no longer needed; never use broad process names, globs, or
directory searches for teardown.

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
| Application object storage | Optional RustFS microVM | Application/storage operators; RustFS availability, backup, TLS and upgrades are separate concerns |

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

- `assets/vmlinux-6.18`
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

## Clock alignment and preserved node restarts

After a laptop/WSL suspension, compare the caller's UTC time, the node's
`/status` timestamp, PostgreSQL `clock_timestamp()` and the RustFS HTTP `Date`
header. A healthy HTTP endpoint alone does not establish clock alignment.
An old node timestamp can make its authoritative registry entry appear expired
and block canonical secret distribution; it can also invalidate JWT or signed
storage requests. Do not bypass registry checks or extend credentials to hide
the discrepancy.

The node image builder synchronizes before startup and runs Chrony with
`makestep 0.1 -1`, permitting correction on later updates after host suspension.
The PostgreSQL fixture already permits such steps; inspect each VM before
restarting unrelated services. These public time sources are local-test
defaults, subject to the production time-source requirements below.

`vm-testbed-cli restart-node` clones the configured base rootfs. It does not
automatically preserve later mutations in the running node's rootfs. For a
stateful rehearsal, stop the exact recorded node, preserve its current disk and
stable seal-key source offline, and use a checked copy as the next base. Never
write into an active guest filesystem. Resolve symlinked paths when editing an
offline copy (`/sbin` may point to `/usr/sbin`) and validate both the resulting
file and filesystem before booting. Keep all credential-bearing copies private.

Size the disk for the deployed component set. The approval rehearsal retained
its current state while growing a checked offline node disk from 2 to 4 GiB;
this removed its capacity warning without deleting old application state or
lowering health thresholds. This is local validation, not a production backup
or upgrade guarantee.

The 2026-10-30 agent ecosystem acceptance rehearsal also encountered memory
backpressure after repeated application upgrades: the existing 2-GiB guest used
1,408 MiB against its configured 1,536-MiB budget. Its exact recorded node was
stopped, its current disk and stable seal-key source were preserved offline, and
a checked private copy was restarted with 4 GiB of physical memory and a 3-GiB
application/admission budget. The 90% health/backpressure threshold remained in
force. This changed only that private one-node fixture, not the image-builder
defaults or a production capacity guarantee. NATS, PostgreSQL and RustFS service
VMs were preserved. The harness project's docs/ecosystem-acceptance.md records
the application validation and resulting resource envelope.
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

The disposable image refuses to start PostgreSQL until Chrony completes an
initial synchronization, keeps Chrony running, and writes tracking/source
snapshots to the recorded serial log every 30 seconds. Its two public NTP
servers are only local-test defaults. Production database hosts require at
least three independent, operator-controlled time sources on the management
network, authenticated NTP/NTS where available, a documented maximum offset,
and paging on both excessive offset and missing clock telemetry.

Chrony's command socket is created in a `chrony:chrony` mode-`0770` directory;
Alpine Chrony disables the socket when the directory is merely group-accessible
but owned by root. After a suspend or source-loss episode, the observer resets
stale measurements once, then issues measurement bursts without resetting the
new samples on every loop. Repeating `reset sources` prevents source selection
from converging. The local recovery drill is
`scripts/vm/validate-postgres-clock.sh`.

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

## Attach application object storage (RustFS)

The optional RustFS fixture uses a separate service microVM, normally
`172.20.0.30:9000`. It does not increase the platform-node count. Its exact VM
identity, PID, IP and rootfs are recorded by the existing testbed CLI in the same
state as the PostgreSQL service. No Docker/Podman storage daemon is left running
on the host; Podman is used only to extract an explicitly pinned OCI filesystem.

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target
export OUTPUT_DIR="$HOME/.local/state/my-private-testbed/rustfs"
export RUSTFS_IMAGE='docker.io/rustfs/rustfs@sha256:<reviewed-image-digest>'
bash scripts/vm/build-rustfs-rootfs.sh
bash scripts/vm/provision-rustfs-service.sh \
  --state-file /path/to/testbed.json \
  --rootfs "$OUTPUT_DIR/rustfs-rootfs.ext4"
```

The image must contain the Alpine RustFS binary, BusyBox and network tools.
BusyBox must include `ntpd` and `timeout`. The guest synchronizes its clock
before starting RustFS and keeps an NTP client running: stale guest time after
WSL suspension can otherwise cause S3 SigV4 requests to fail despite a healthy
TCP/HTTP endpoint. The disposable image uses public NTP peers; production time
sources and reachability are operator responsibilities. Compare the RustFS
HTTP `Date` header with the caller's UTC clock when diagnosing signed requests.
The builder records the immutable image reference, creates unique root
credentials in mode-0600 files and seals a mode-0600 rootfs on the Linux
filesystem. It refuses to overwrite an existing image, including an offline
image that might contain data. Reuse the recorded healthy service; inspect the
state and logs before any replacement. WSL privilege escalation uses the WSL
root launcher, with a noninteractive sudo fallback on other Linux hosts.

Treat the generated rootfs as a credential-bearing local fixture, not a
publishable or reusable production base. Its writable `/data` also holds
uploaded documents. Keep both image and credential files private. The canonical
recorded teardown stops the exact service process/network resources; retain the
image until the user explicitly authorizes deleting its data. Do not delete it
by filename pattern or rebuild over it while the service is live.

Provision the bucket and application IAM user separately using authenticated
RustFS administration. Reserve the root credential for bootstrap; the document
application should get only `s3:GetObject` and `s3:PutObject` on its own object
prefix. Check denied bucket listing, deletion and writes outside that prefix.
Inject application credentials through platform secret references, never literal
manifest environment values. The fixture disables the web console and uses
HTTP on the private test network; production requires independently operated
DNS, TLS, IAM lifecycle, backup and availability controls.

The platform's restrictive HTTP profile authorizes exact DNS hostnames plus
explicit destination CIDRs. Do not put IP literals in `allowed_http_hosts` or
repurpose `.internal` for a remote storage service: `.internal` remains the
node-local application mesh. The single-node document rehearsal used
`rustfs.172.20.0.30.sslip.io` pinned by policy to `172.20.0.30/32`; this introduces
a public DNS dependency only for that local fixture. Replace it with the
operator's own DNS and TLS endpoint outside the rehearsal. Bucket creation,
IAM and application migrations are provisioning work, never request-time work.

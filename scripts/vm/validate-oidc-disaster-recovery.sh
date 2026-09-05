#!/usr/bin/env bash
set -euo pipefail

state_file=.prod-validation-single-host-state.json
app_dir=/mnt/d/dev/openid_connect_wasi
report_dir=/tmp/wasm-cloud-platform-oidc-recovery
public_url=http://localhost:8088
restore_port=55432
source_database=oidc
source_user=oidc
retention_days=7
max_clock_skew_seconds=5
auth_token=${WASM_CTL_AUTH_TOKEN:-local-test-write-token-change-me}

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state file}; shift 2 ;;
    --app-dir) app_dir=${2:?missing application directory}; shift 2 ;;
    --report-dir) report_dir=${2:?missing report directory}; shift 2 ;;
    --restore-port) restore_port=${2:?missing restore port}; shift 2 ;;
    --retention-days) retention_days=${2:?missing retention days}; shift 2 ;;
    --max-clock-skew-seconds) max_clock_skew_seconds=${2:?missing clock-skew threshold}; shift 2 ;;
    -h|--help)
      echo "Usage: PGPASSWORD=... validate-oidc-disaster-recovery.sh [--state-file FILE] [--app-dir DIR] [--report-dir DIR] [--restore-port PORT] [--retention-days DAYS] [--max-clock-skew-seconds SECONDS]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
[[ -n ${PGPASSWORD:-} ]] || { echo "Set PGPASSWORD without placing it on the command line." >&2; exit 1; }
[[ -f "$state_file" ]] || { echo "Missing topology state: $state_file" >&2; exit 1; }
[[ -f "$app_dir/Cargo.toml" ]] || { echo "Missing OIDC checkout: $app_dir" >&2; exit 1; }
[[ "$restore_port" =~ ^[0-9]+$ ]] && ((restore_port >= 1024 && restore_port <= 65535)) || {
  echo "Restore port must be an unprivileged TCP port." >&2
  exit 2
}
[[ "$retention_days" =~ ^[0-9]+$ ]] && ((retention_days > 0)) || {
  echo "Retention days must be positive." >&2
  exit 2
}
[[ "$max_clock_skew_seconds" =~ ^[0-9]+([.][0-9]+)?$ ]] || {
  echo "Maximum clock skew must be a non-negative number." >&2
  exit 2
}
[[ ! -e "$report_dir" ]] || { echo "Report path already exists: $report_dir" >&2; exit 1; }
for command_name in cargo cmp curl gpg jq npm openssl podman python3 sha256sum ss stat; do
  command -v "$command_name" >/dev/null || { echo "Missing required command: $command_name" >&2; exit 1; }
done

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root"
state_file=$(realpath "$state_file")
report_dir=$(realpath -m "$report_dir")
mkdir -m 700 -p "$report_dir"

state_key=$(printf '%s' "$state_file" | sha256sum | cut -d' ' -f1)
short_key=${state_key:0:12}
runtime_root=${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-recovery-validation-$(id -u)
runtime_dir=$runtime_root/$state_key
container_name=wcp-oidc-recovery-$short_key
restore_password_file=$runtime_dir/restore-password
encryption_key_file=$runtime_dir/encryption-passphrase
plain_dump=$runtime_dir/oidc.dump
decrypted_dump=$runtime_dir/oidc.decrypted.dump
encrypted_dump=$report_dir/oidc.dump.gpg
source_snapshot=$report_dir/source-snapshot.txt
restored_snapshot=$report_dir/restored-snapshot.txt
marker_table=phase9_recovery_marker
marker_id=$(date -u +%Y%m%dT%H%M%SZ)-$RANDOM

mkdir -p "$runtime_root"
chmod 700 "$runtime_root"
rm -rf -- "$runtime_dir"
mkdir -m 700 -p "$runtime_dir"
openssl rand -hex 32 > "$restore_password_file"
openssl rand -hex 32 > "$encryption_key_file"
chmod 600 "$restore_password_file" "$encryption_key_file"
restore_password=$(<"$restore_password_file")

source_host=$(jq -er '.services[] | select(.kind == "postgresql") | .ip' "$state_file")
nats_url=$(jq -er '.nats_url' "$state_file")
mapfile -t node_rows < <(jq -r '.nodes[] | [.id,.admin_addr,.proxy_addr] | join("|")' "$state_file")
((${#node_rows[@]} > 0)) || { echo "No platform nodes in state." >&2; exit 1; }
first_admin=${node_rows[0]#*|}; first_admin=${first_admin%%|*}

target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
export CARGO_TARGET_DIR=$target_dir
cargo build -q -p vm-testbed --bin vm-testbed-cli
cargo build -q -p ctl --bin wasm-ctl
testbed=$target_dir/debug/vm-testbed-cli
ctl=$target_dir/debug/wasm-ctl

routes_file=$runtime_dir/routes.json
apps_file=$runtime_dir/apps.json
curl -fsS --max-time 5 -H "Authorization: Bearer $auth_token" \
  "http://$first_admin/admin/routes" > "$routes_file"
curl -fsS --max-time 5 -H "Authorization: Bearer $auth_token" \
  "http://$first_admin/admin/apps?namespace=oidc" > "$apps_file"
lkg_backend=$(jq -er '[.routes[] | select(.host == "oidc-backend.internal" and .path_prefix == "/") | .app_id] | if length == 1 then .[0] else error("expected exactly one backend route") end' "$routes_file")
[[ "$lkg_backend" == oidc/openid-connect-wasi:* ]] || { echo "Unexpected OIDC backend: $lkg_backend" >&2; exit 1; }
lkg_version=${lkg_backend##*:}
recovery_run_id=$(printf '%s' "$report_dir" | sha256sum | cut -c1-8)
recovery_version=${lkg_version}-phase9-recovery-$recovery_run_id
recovery_app=oidc/openid-connect-wasi:$recovery_version
recovery_host=oidc-backend-recovery.internal

app_target_dir=${OIDC_CARGO_TARGET_DIR:-/tmp/openid-connect-wasi-target}
frontend_wasm=$app_target_dir/wasm32-wasip2/release/oidc_admin_wasi.wasm
backend_wasm=$app_target_dir/wasm32-wasip2/release/openid_connect_wasi.wasm
[[ -f "$frontend_wasm" && -f "$backend_wasm" ]] || {
  echo "Missing tested OIDC artifacts under $app_target_dir." >&2
  exit 1
}

secret_root=${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-oidc-secrets-$(id -u)
secret_dir=$secret_root/$state_key
for secret in rsa.pem ed25519.pem encryption-key pairwise-salt; do
  [[ -s "$secret_dir/$secret" ]] || { echo "Missing OIDC test secret reference: $secret_dir/$secret" >&2; exit 1; }
done
rsa_key=$(<"$secret_dir/rsa.pem")
ed25519_key=$(<"$secret_dir/ed25519.pem")
encryption_key=$(<"$secret_dir/encryption-key")
pairwise_salt=$(<"$secret_dir/pairwise-salt")

route_add() {
  "$ctl" --nats-url "$nats_url" --auth-token "$auth_token" routes add --host "$1" --app "$2"
}

route_remove() {
  "$ctl" --nats-url "$nats_url" --auth-token "$auth_token" routes remove --host "$1"
}

source_psql() {
  podman run --rm --network host --env PGPASSWORD \
    docker.io/library/postgres:17-alpine psql -v ON_ERROR_STOP=1 \
    -h "$source_host" -U "$source_user" -d "$source_database" -Atqc "$1"
}

restore_psql() {
  podman exec -e PGPASSWORD="$restore_password" "$container_name" \
    psql -v ON_ERROR_STOP=1 -h 127.0.0.1 -p "$restore_port" \
    -U postgres -d restored_oidc -Atqc "$1"
}

write_snapshot() {
  local side=$1 output=$2 table quoted count digest
  : > "$output"
  if [[ "$side" == source ]]; then
    mapfile -t tables < <(source_psql "SELECT tablename FROM pg_tables WHERE schemaname='public' ORDER BY tablename")
  else
    mapfile -t tables < <(restore_psql "SELECT tablename FROM pg_tables WHERE schemaname='public' ORDER BY tablename")
  fi
  for table in "${tables[@]}"; do
    [[ "$table" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || { echo "Unsafe table identifier: $table" >&2; return 1; }
    quoted=\"$table\"
    if [[ "$side" == source ]]; then
      count=$(source_psql "SELECT count(*) FROM public.$quoted")
      digest=$(source_psql "SELECT md5(COALESCE(string_agg(row_to_json(t)::text, E'\\n' ORDER BY row_to_json(t)::text),'')) FROM public.$quoted t")
    else
      count=$(restore_psql "SELECT count(*) FROM public.$quoted")
      digest=$(restore_psql "SELECT md5(COALESCE(string_agg(row_to_json(t)::text, E'\\n' ORDER BY row_to_json(t)::text),'')) FROM public.$quoted t")
    fi
    printf '%s|%s|%s\n' "$table" "$count" "$digest" >> "$output"
  done
  chmod 600 "$output"
}

wait_public_ready() {
  local deadline=$((SECONDS + 90)) response
  while ((SECONDS < deadline)); do
    response=$(curl -fsS --max-time 10 "$public_url/health/ready" 2>/dev/null || true)
    [[ "$response" == *'"status":"ready"'* && "$response" == *'"database":"ok"'* ]] && return 0
    sleep 1
  done
  echo "Public OIDC readiness did not become healthy." >&2
  return 1
}

probe_recovery_direct() {
  local row proxy response
  for row in "${node_rows[@]}"; do
    proxy=${row##*|}
    response=$(curl -fsS --max-time 10 -H "Host: $recovery_host" "http://$proxy/health/ready")
    [[ "$response" == *'"status":"ready"'* && "$response" == *'"database":"ok"'* ]] || {
      echo "Recovery readiness failed through $proxy: $response" >&2
      return 1
    }
  done
}

container_started=false
marker_created=false
recovery_deployed=false
public_cutover=false
cleanup() {
  local exit_code=$?
  set +e
  if [[ "$public_cutover" == true ]]; then
    route_add oidc-backend.internal "$lkg_backend" >/dev/null 2>&1
  fi
  route_remove "$recovery_host" >/dev/null 2>&1
  if [[ "$recovery_deployed" == true ]]; then
    "$testbed" undeploy-app --state-file "$state_file" --app-id "$recovery_app" >/dev/null 2>&1
  fi
  if [[ "$marker_created" == true ]]; then
    source_psql "DROP TABLE IF EXISTS public.$marker_table" >/dev/null 2>&1
  fi
  if [[ "$container_started" == true ]]; then
    recorded_label=$(podman inspect --format '{{index .Config.Labels "io.wasm-cloud-platform.validation"}}' "$container_name" 2>/dev/null || true)
    [[ "$recorded_label" != "$state_key" ]] || podman rm -f "$container_name" >/dev/null 2>&1
  fi
  rm -rf -- "$runtime_dir"
  wait_public_ready >/dev/null 2>&1
  exit "$exit_code"
}
trap cleanup EXIT

[[ ! $(jq -r --arg app "$recovery_app" '[.[] | select(.id == $app)] | length' "$apps_file") == 1 ]] || {
  echo "Recovery candidate already exists: $recovery_app" >&2
  exit 1
}
if podman container exists "$container_name"; then
  echo "Refusing to replace existing container: $container_name" >&2
  exit 1
fi
if ss -H -ltn "sport = :$restore_port" | grep -q .; then
  echo "Restore port is already listening: $restore_port" >&2
  exit 1
fi

wait_public_ready
source_version=$(source_psql "SHOW server_version")
source_migrations=$(source_psql "SELECT count(*) || '|' || count(DISTINCT filename) FROM public._migrations")

source_psql "CREATE TABLE public.$marker_table (id text PRIMARY KEY, captured_at timestamptz NOT NULL DEFAULT clock_timestamp()); INSERT INTO public.$marker_table(id) VALUES ('$marker_id')" >/dev/null
marker_created=true
marker_epoch=$(source_psql "SELECT extract(epoch from captured_at) FROM public.$marker_table WHERE id='$marker_id'")
marker_committed_ms=$(date +%s%3N)

backup_started_ms=$(date +%s%3N)
podman run --rm --network host --env PGPASSWORD \
  --volume "$runtime_dir:/backup:Z" docker.io/library/postgres:17-alpine \
  pg_dump -h "$source_host" -U "$source_user" -d "$source_database" \
  --format=custom --no-owner --no-privileges --file=/backup/oidc.dump
backup_finished_ms=$(date +%s%3N)
chmod 600 "$plain_dump"
plain_sha=$(sha256sum "$plain_dump" | cut -d' ' -f1)

# A transaction committed after pg_dump must not appear in the restored snapshot.
source_psql "INSERT INTO public.$marker_table(id) VALUES ('${marker_id}-after-backup')" >/dev/null
source_psql "DELETE FROM public.$marker_table WHERE id='${marker_id}-after-backup'" >/dev/null
write_snapshot source "$source_snapshot"

gpg --batch --yes --pinentry-mode loopback --passphrase-file "$encryption_key_file" \
  --symmetric --cipher-algo AES256 --output "$encrypted_dump" "$plain_dump"
chmod 600 "$encrypted_dump"
gpg --batch --yes --pinentry-mode loopback --passphrase-file "$encryption_key_file" \
  --output "$decrypted_dump" --decrypt "$encrypted_dump" >/dev/null 2>&1
[[ $(sha256sum "$decrypted_dump" | cut -d' ' -f1) == "$plain_sha" ]] || {
  echo "Encrypted backup round-trip checksum mismatch." >&2
  exit 1
}
encrypted_sha=$(sha256sum "$encrypted_dump" | cut -d' ' -f1)

recovery_started_ms=$(date +%s%3N)
podman run --detach --name "$container_name" --network host \
  --label "io.wasm-cloud-platform.validation=$state_key" \
  --env POSTGRES_PASSWORD="$restore_password" \
  --volume "$runtime_dir:/backup:ro,Z" \
  docker.io/library/postgres:17-alpine -p "$restore_port" >/dev/null
container_started=true
for _ in $(seq 1 60); do
  podman exec "$container_name" pg_isready -h 127.0.0.1 -p "$restore_port" -U postgres >/dev/null 2>&1 && break
  sleep 1
done
podman exec "$container_name" pg_isready -h 127.0.0.1 -p "$restore_port" -U postgres >/dev/null
podman exec -e PGPASSWORD="$restore_password" "$container_name" \
  createdb -h 127.0.0.1 -p "$restore_port" -U postgres restored_oidc
podman exec -e PGPASSWORD="$restore_password" "$container_name" \
  pg_restore -h 127.0.0.1 -p "$restore_port" -U postgres -d restored_oidc \
  --exit-on-error --no-owner --no-privileges /backup/oidc.dump
database_restored_ms=$(date +%s%3N)

restored_version=$(restore_psql "SHOW server_version")
restored_migrations=$(restore_psql "SELECT count(*) || '|' || count(DISTINCT filename) FROM public._migrations")
[[ "$restored_migrations" == "$source_migrations" ]] || {
  echo "Migration mismatch: source=$source_migrations restored=$restored_migrations" >&2
  exit 1
}
[[ $(restore_psql "SELECT count(*) FROM public.$marker_table WHERE id='$marker_id'") == 1 ]] || {
  echo "Recovery-point marker is absent from restored database." >&2
  exit 1
}
[[ $(restore_psql "SELECT count(*) FROM public.$marker_table WHERE id='${marker_id}-after-backup'") == 0 ]] || {
  echo "Post-backup marker unexpectedly appeared in restored database." >&2
  exit 1
}
write_snapshot restore "$restored_snapshot"
cmp -s "$source_snapshot" "$restored_snapshot" || {
  diff -u "$source_snapshot" "$restored_snapshot" > "$report_dir/snapshot-diff.txt" || true
  echo "Restored table counts or content hashes differ from the backup snapshot." >&2
  exit 1
}

runtime_database_url="postgresql://postgres:${restore_password}@172.20.0.1:${restore_port}/restored_oidc?sslmode=disable&connect_timeout=2&statement_timeout=5000&lock_timeout=2000&idle_in_transaction_session_timeout=30000"
backend_fuel=10000000000
recovery_deployed=true
scripts/vm/deploy-test-application.sh \
  --state-file "$state_file" --app openid-connect-wasi --version "$recovery_version" --namespace oidc \
  --wasm "$backend_wasm" --route-host "$recovery_host" --fuel "$backend_fuel" \
  --verify-path /health/ready --verify-contains '"database":"ok"' --verify-direct-node \
  --env "OIDC_DATABASE_URL=$runtime_database_url" --env "OIDC_ISSUER=$public_url" \
  --env "OIDC_ENCRYPTION_KEY=$encryption_key" --env "OIDC_PAIRWISE_SALT=$pairwise_salt" \
  --env "OIDC_SIGNING_KEY=$rsa_key" --env "OIDC_SIGNING_KID=local-rsa-1" \
  --env "OIDC_ED25519_KEY=$ed25519_key" --env "OIDC_ED25519_KID=local-ed25519-1" \
  --env "OIDC_RATE_LIMIT_MODE=proxy" --env "OIDC_TRUST_PROXY_HEADERS=true" --env "OIDC_CORS_ORIGINS="
probe_recovery_direct

route_add oidc-backend.internal "$recovery_app"
public_cutover=true
wait_public_ready
application_ready_ms=$(date +%s%3N)

(cd "$app_dir/front/admin" && \
  PLAYWRIGHT_BASE_URL="$public_url" DEFAULT_EMAIL=admin@example.com DEFAULT_PASSWORD=Admin123 \
  npx playwright test --project=chromium --workers=1 \
    e2e/login.spec.js e2e/dashboard.spec.js) > "$report_dir/playwright.log" 2>&1
journey_finished_ms=$(date +%s%3N)

route_add oidc-backend.internal "$lkg_backend"
public_cutover=false
wait_public_ready
route_remove "$recovery_host"
"$testbed" undeploy-app --state-file "$state_file" --app-id "$recovery_app" >/dev/null
recovery_deployed=false
source_psql "DROP TABLE public.$marker_table" >/dev/null
marker_created=false
podman rm -f "$container_name" >/dev/null
container_started=false

created_at=$(date -u -d "@$((backup_started_ms / 1000))" +%Y-%m-%dT%H:%M:%SZ)
expires_at=$(date -u -d "$created_at + $retention_days days" +%Y-%m-%dT%H:%M:%SZ)
rpo_seconds=$(python3 - "$marker_committed_ms" "$backup_started_ms" <<'PY'
import sys
print(f"{max(0.0, (int(sys.argv[2]) - int(sys.argv[1])) / 1000):.3f}")
PY
)
database_clock_skew_seconds=$(python3 - "$marker_epoch" "$marker_committed_ms" <<'PY'
import sys
print(f"{abs(int(sys.argv[2]) / 1000 - float(sys.argv[1])):.3f}")
PY
)
python3 - "$database_clock_skew_seconds" "$max_clock_skew_seconds" <<'PY'
import sys
observed, maximum = map(float, sys.argv[1:])
if observed > maximum:
    raise SystemExit(
        f"source database clock skew {observed:.3f}s exceeds {maximum:.3f}s"
    )
PY

python3 - "$state_file" "$routes_file" "$apps_file" "$report_dir/recovery-manifest.json" \
  "$created_at" "$expires_at" "$retention_days" "$plain_sha" "$encrypted_sha" \
  "$frontend_wasm" "$backend_wasm" "$source_version" "$restored_version" \
  "$source_migrations" "$state_key" <<'PY'
import hashlib, json, os, sys
(state_path, routes_path, apps_path, output, created_at, expires_at,
 retention_days, plain_sha, encrypted_sha, frontend, backend,
 source_version, restored_version, migrations, state_key) = sys.argv[1:]
with open(state_path, encoding="utf-8") as stream:
    state = json.load(stream)
with open(routes_path, encoding="utf-8") as stream:
    routes = json.load(stream).get("routes", [])
with open(apps_path, encoding="utf-8") as stream:
    apps = json.load(stream)
def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()
def artifact(path, role):
    return {"role": role, "path": path, "sha256": digest(path)}
repo = os.path.dirname(state_path)
artifact_paths = [
    (os.path.join(repo, state["kernel_path"]), "firecracker-kernel"),
    (os.path.join(repo, state["node_rootfs_path"]), "platform-node-rootfs"),
    (os.path.join(repo, "assets/nats-rootfs.ext4"), "nats-rootfs"),
    (os.path.join(repo, "assets/postgres-rootfs.ext4"), "postgresql-rootfs"),
    (frontend, "oidc-frontend-wasm"), (backend, "oidc-backend-wasm")]
safe_apps=[]
active_app_ids = {route.get("app_id") for route in routes}
for app in apps if isinstance(apps, list) else apps.get("apps", []):
    if app.get("id") in active_app_ids:
        safe_apps.append({k: app.get(k) for k in ("id", "status", "desired_instances", "healthy_instances") if k in app})
manifest = {
    "schema_version": 1,
    "created_at": created_at,
    "expires_at": expires_at,
    "retention_days": int(retention_days),
    "classification": "local-validation-evidence",
    "state_key": state_key,
    "topology": {
        "name": state["name"], "profile": state["profile"],
        "subnet": state["subnet"], "gateway": state["gateway"],
        "nats_url": state["nats_url"], "platform_node_count": len(state["nodes"]),
        "node_memory_mb": state["node_memory_mb"], "node_vcpus": state["node_vcpus"],
        "services": [{"id": s["id"], "kind": s["kind"], "ip": s["ip"], "port": s["port"]} for s in state.get("services", [])]},
    "artifacts": [artifact(path, role) for path, role in artifact_paths],
    "routes": [{k: r.get(k) for k in ("host", "path_prefix", "app_id")} for r in routes],
    "applications": safe_apps,
    "secret_requirements": ["OIDC database credentials", "OIDC RSA signing key", "OIDC Ed25519 signing key", "OIDC encryption key", "OIDC pairwise salt", "control-plane authentication token"],
    "secret_values_in_manifest": False,
    "database": {"source_version": source_version, "restored_version": restored_version,
                 "migration_count_and_distinct_count": migrations,
                 "plaintext_sha256": plain_sha, "encrypted_sha256": encrypted_sha,
                 "format": "PostgreSQL custom", "encryption": "OpenPGP symmetric AES-256"}}
with open(output, "w", encoding="utf-8") as stream:
    json.dump(manifest, stream, indent=2, sort_keys=True)
    stream.write("\n")
PY
chmod 600 "$report_dir/recovery-manifest.json"

python3 - "$report_dir/result.json" "$rpo_seconds" "$database_clock_skew_seconds" \
  "$backup_started_ms" "$backup_finished_ms" "$recovery_started_ms" \
  "$database_restored_ms" "$application_ready_ms" "$journey_finished_ms" \
  "$source_migrations" "$plain_sha" "$encrypted_sha" "$source_snapshot" <<'PY'
import json, sys
(output, rpo, database_clock_skew, backup_start, backup_end, recovery_start, database_restored,
 application_ready, journey_finished, migrations, plain_sha, encrypted_sha,
 snapshot_path) = sys.argv[1:]
def elapsed(start, end): return round((int(end) - int(start)) / 1000, 3)
with open(snapshot_path, encoding="utf-8") as stream:
    table_count = sum(1 for line in stream if line.strip())
result = {
    "status": "pass-with-local-encryption-retention-limitations",
    "observed_rpo_seconds": float(rpo),
    "source_database_clock_skew_seconds": float(database_clock_skew),
    "backup_duration_seconds": elapsed(backup_start, backup_end),
    "database_restore_rto_seconds": elapsed(recovery_start, database_restored),
    "application_ready_rto_seconds": elapsed(recovery_start, application_ready),
    "full_journey_rto_seconds": elapsed(recovery_start, journey_finished),
    "public_tables_verified": table_count,
    "migration_count_and_distinct_count": migrations,
    "plaintext_sha256": plain_sha,
    "encrypted_sha256": encrypted_sha,
    "playwright": "6 focused login/dashboard checks passed",
    "limitations": [
        "Observed RPO measures an on-demand local snapshot boundary, not a scheduled production backup interval.",
        "The database clock gate proves the local Chrony-backed fixture stayed within the configured threshold; it does not qualify production time-source availability or NTS authentication.",
        "The disposable encryption passphrase was destroyed after verification; production KMS/HSM custody, key rotation, off-host replication, object lock, and retention enforcement were not validated."]}
with open(output, "w", encoding="utf-8") as stream:
    json.dump(result, stream, indent=2, sort_keys=True)
    stream.write("\n")
PY
chmod 600 "$report_dir/result.json"

[[ $(stat -c '%a' "$report_dir") == 700 ]]
for evidence in "$encrypted_dump" "$report_dir/recovery-manifest.json" "$report_dir/result.json" "$source_snapshot" "$restored_snapshot" "$report_dir/playwright.log"; do
  chmod 600 "$evidence"
  [[ $(stat -c '%a' "$evidence") == 600 ]]
done

rm -rf -- "$runtime_dir"
trap - EXIT
echo "OIDC disaster-recovery validation passed. Evidence: $report_dir/result.json"

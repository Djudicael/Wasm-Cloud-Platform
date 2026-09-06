#!/usr/bin/env bash
set -euo pipefail

state_file=.prod-validation-single-host-state.json
app_dir=/mnt/d/dev/openid_connect_wasi
report_dir=/tmp/wasm-cloud-platform-oidc-upgrade
public_url=http://127.0.0.1:8088
auth_token=${WASM_CTL_AUTH_TOKEN:-local-test-write-token-change-me}
database_url='postgresql://oidc:oidc-local-test@172.20.0.20:5432/oidc?sslmode=disable'

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state file}; shift 2 ;;
    --app-dir) app_dir=${2:?missing application directory}; shift 2 ;;
    --report-dir) report_dir=${2:?missing report directory}; shift 2 ;;
    -h|--help)
      echo "Usage: validate-oidc-upgrade-rollback.sh [--state-file FILE] [--app-dir DIR] [--report-dir DIR]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
[[ -f "$state_file" ]] || { echo "Missing state file: $state_file" >&2; exit 1; }
[[ -f "$app_dir/Cargo.toml" ]] || { echo "Missing OIDC checkout: $app_dir" >&2; exit 1; }
[[ ! -e "$report_dir" ]] || { echo "Report path already exists: $report_dir" >&2; exit 1; }
mkdir -m 700 -p "$report_dir"

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root"
state_file=$(python3 -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' "$state_file")
target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
export CARGO_TARGET_DIR=$target_dir
cargo build -p vm-testbed --bin vm-testbed-cli --bin http-benchmark
cargo build -p ctl --bin wasm-ctl
testbed="$target_dir/debug/vm-testbed-cli"
benchmark="$target_dir/debug/http-benchmark"
ctl="$target_dir/debug/wasm-ctl"

mapfile -t topology < <(python3 - "$state_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    state=json.load(stream)
print(state["nats_url"])
for node in state["nodes"]:
    print(node["id"] + "|" + node["admin_addr"] + "|" + node["proxy_addr"])
PY
)
nats_url=${topology[0]}
node_rows=("${topology[@]:1}")
first_admin=${node_rows[0]#*|}; first_admin=${first_admin%%|*}

routes=$(curl -fsS --max-time 5 -H "Authorization: Bearer $auth_token" "http://$first_admin/admin/routes")
lkg_backend=$(python3 - "$routes" <<'PY'
import json, sys
routes=json.loads(sys.argv[1])["routes"]
matches=[r["app_id"] for r in routes if r["host"] == "oidc-backend.internal" and r["path_prefix"] == "/"]
if len(matches) != 1:
    raise SystemExit(f"expected one OIDC backend route, found {matches}")
print(matches[0])
PY
)
[[ "$lkg_backend" == oidc/openid-connect-wasi:* ]] || { echo "Unexpected rollback target: $lkg_backend" >&2; exit 1; }
lkg_version=${lkg_backend##*:}
candidate_version="${lkg_version}-phase8-candidate"
failed_version="${lkg_version}-phase8-failed"
candidate_app="oidc/openid-connect-wasi:$candidate_version"
failed_app="oidc/openid-connect-wasi:$failed_version"

app_target_dir=${OIDC_CARGO_TARGET_DIR:-/tmp/openid-connect-wasi-target}
backend_wasm="$app_target_dir/wasm32-wasip2/release/openid_connect_wasi.wasm"
[[ -f "$backend_wasm" ]] || { echo "Missing tested backend artifact: $backend_wasm" >&2; exit 1; }
artifact_sha=$(sha256sum "$backend_wasm" | cut -d' ' -f1)

secret_root="${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-oidc-secrets-$(id -u)"
state_key=$(printf '%s' "$state_file" | sha256sum | cut -d' ' -f1)
secret_dir="$secret_root/$state_key"
for secret in rsa.pem ed25519.pem encryption-key pairwise-salt; do
  [[ -s "$secret_dir/$secret" ]] || { echo "Missing OIDC test secret: $secret_dir/$secret" >&2; exit 1; }
done
rsa_key=$(<"$secret_dir/rsa.pem")
ed25519_key=$(<"$secret_dir/ed25519.pem")
encryption_key=$(<"$secret_dir/encryption-key")
pairwise_salt=$(<"$secret_dir/pairwise-salt")
runtime_database_url="${database_url}&connect_timeout=2&statement_timeout=5000&lock_timeout=2000&idle_in_transaction_session_timeout=30000"
backend_fuel=10000000000

route_add() {
  "$ctl" --nats-url "$nats_url" --auth-token "$auth_token" routes add --host "$1" --app "$2"
}

route_remove() {
  "$ctl" --nats-url "$nats_url" --auth-token "$auth_token" routes remove --host "$1"
}

undeploy_if_present() {
  "$testbed" undeploy-app --state-file "$state_file" --app-id "$1" >/dev/null 2>&1 || true
}

probe_direct() {
  local host=$1 path=$2
  local row proxy response
  for row in "${node_rows[@]}"; do
    proxy=${row##*|}
    response=$(curl -fsS --max-time 10 -H "Host: $host" "http://$proxy$path")
    [[ "$response" == *'"status":"ready"'* && "$response" == *'"database":"ok"'* ]] || {
      echo "Unexpected response from $proxy for $host$path: $response" >&2
      return 1
    }
  done
}

check_guest_clocks() {
  local output=$1 row node proxy headers guest_date guest_epoch host_epoch skew
  : > "$output"
  for row in "${node_rows[@]}"; do
    node=${row%%|*}
    proxy=${row##*|}
    headers=$(curl -fsS --max-time 10 -D - -o /dev/null -H 'Host: oidc-backend.internal' \
      "http://$proxy/health/ready")
    guest_date=$(awk 'BEGIN{IGNORECASE=1} /^date:/ {sub(/^[^:]+:[[:space:]]*/, ""); sub(/\r$/, ""); print; exit}' <<<"$headers")
    [[ -n "$guest_date" ]] || { echo "Missing Date header from $node" >&2; return 1; }
    guest_epoch=$(date -u -d "$guest_date" +%s)
    host_epoch=$(date -u +%s)
    skew=$((host_epoch - guest_epoch)); ((skew < 0)) && skew=$((-skew))
    printf '%s|%s|%s\n' "$node" "$skew" "$guest_date" >> "$output"
    ((skew <= 5)) || {
      echo "$node clock differs from the load generator by ${skew}s" >&2
      return 1
    }
  done
}

wait_public_ready() {
  local deadline=$((SECONDS + 60)) response
  while ((SECONDS < deadline)); do
    response=$(curl -fsS --max-time 10 "$public_url/health/ready" 2>/dev/null || true)
    [[ "$response" == *'"status":"ready"'* && "$response" == *'"database":"ok"'* ]] && return 0
    sleep 1
  done
  echo "Public OIDC readiness did not recover" >&2
  return 1
}

psql_exec() {
  podman run --rm --network host -e PGPASSWORD=oidc-local-test docker.io/library/postgres:17-alpine \
    psql -v ON_ERROR_STOP=1 -h 172.20.0.20 -U oidc -d oidc -Atqc "$1"
}

candidate_deployed=false
failed_deployed=false
schema_probe=false
load_pid=
cleanup() {
  local exit_code=$?
  set +e
  route_add oidc-backend.internal "$lkg_backend" >/dev/null 2>&1
  route_remove oidc-backend-lkg.internal >/dev/null 2>&1
  route_remove oidc-backend-candidate.internal >/dev/null 2>&1
  route_remove oidc-backend-failed.internal >/dev/null 2>&1
  [[ "$failed_deployed" == false ]] || undeploy_if_present "$failed_app"
  [[ "$candidate_deployed" == false ]] || undeploy_if_present "$candidate_app"
  if [[ "$schema_probe" == true ]]; then
    psql_exec "BEGIN; DELETE FROM _migrations WHERE filename='V9000__phase8_additive_probe.sql'; DROP TABLE IF EXISTS phase8_upgrade_probe; COMMIT;" >/dev/null
  fi
  if [[ -n "$load_pid" ]]; then
    wait "$load_pid" >/dev/null 2>&1
  fi
  wait_public_ready >/dev/null 2>&1
  exit "$exit_code"
}
trap cleanup EXIT

probe_direct oidc-backend.internal /health/ready
check_guest_clocks "$report_dir/clock-skew-before.txt"
wait_public_ready
migrations_before=$(psql_exec "SELECT count(*) || '|' || count(DISTINCT filename) FROM _migrations")

cat > "$report_dir/baseline.json" <<EOF
{"lkg_backend":"$lkg_backend","candidate_backend":"$candidate_app","artifact_sha256":"$artifact_sha","migrations":"$migrations_before"}
EOF
chmod 600 "$report_dir/baseline.json"

"$benchmark" --url "$public_url/health/ready" --host localhost --requests 450 \
  --concurrency 10 --warmup-requests 2 --rate-per-second 5 --expected-status 200 \
  > "$report_dir/upgrade-load.json" 2> "$report_dir/upgrade-load.stderr" &
load_pid=$!

psql_exec "BEGIN; SELECT pg_advisory_xact_lock(hashtext('oidc_hub_migrations')); CREATE TABLE phase8_upgrade_probe (id bigint PRIMARY KEY, marker text NOT NULL, created_at timestamptz NOT NULL DEFAULT now()); INSERT INTO phase8_upgrade_probe(id,marker) VALUES (1,'expand-compatible'); INSERT INTO _migrations(filename) VALUES ('V9000__phase8_additive_probe.sql') ON CONFLICT DO NOTHING; COMMIT;" >/dev/null
schema_probe=true

candidate_deployed=true
scripts/vm/deploy-test-application.sh \
  --state-file "$state_file" --app openid-connect-wasi --version "$candidate_version" --namespace oidc \
  --wasm "$backend_wasm" --route-host oidc-backend-candidate.internal --fuel "$backend_fuel" \
  --verify-path /health/ready --verify-contains '"database":"ok"' --verify-direct-node \
  --env "OIDC_DATABASE_URL=$runtime_database_url" --env "OIDC_ISSUER=http://localhost:8088" \
  --env "OIDC_ENCRYPTION_KEY=$encryption_key" --env "OIDC_PAIRWISE_SALT=$pairwise_salt" \
  --env "OIDC_SIGNING_KEY=$rsa_key" --env "OIDC_SIGNING_KID=local-rsa-1" \
  --env "OIDC_ED25519_KEY=$ed25519_key" --env "OIDC_ED25519_KID=local-ed25519-1" \
  --env "OIDC_RATE_LIMIT_MODE=proxy" --env "OIDC_TRUST_PROXY_HEADERS=true" --env "OIDC_CORS_ORIGINS="

route_add oidc-backend-lkg.internal "$lkg_backend"
probe_direct oidc-backend-lkg.internal /health/ready
probe_direct oidc-backend-candidate.internal /health/ready

(cd "$app_dir" && OIDC_DATABASE_URL="$database_url" CARGO_TARGET_DIR="$app_target_dir" \
  cargo run -q -p oidc-migrate --release) > "$report_dir/migrator.log" 2>&1
probe_direct oidc-backend-lkg.internal /health/ready
probe_direct oidc-backend-candidate.internal /health/ready

route_add oidc-backend.internal "$candidate_app"
wait_public_ready

set +e
WASM_CTL_AUTH_TOKEN="$auth_token" "$testbed" deploy-app --state-file "$state_file" \
  --app openid-connect-wasi --version "$failed_version" \
  --namespace oidc --wasm "$backend_wasm" --route-host oidc-backend-failed.internal \
  --fuel "$backend_fuel" --health-check-path /health/ready \
  --env OIDC_DATABASE_URL=not-a-valid-database-url \
  --env "OIDC_ISSUER=http://localhost:8088" > "$report_dir/failed-candidate-deploy.log" 2>&1
failed_deploy_exit=$?
set -e
failed_deployed=true
sleep 3
failed_status=$(curl -sS -o "$report_dir/failed-candidate-response" -w '%{http_code}' --max-time 5 \
  -H 'Host: oidc-backend-failed.internal' "http://${node_rows[0]##*|}/health/ready" || true)
[[ "$failed_status" != 200 ]] || { echo "Synthetic failed candidate unexpectedly became ready" >&2; exit 1; }
printf '%s\n' "$failed_deploy_exit|$failed_status" > "$report_dir/failed-candidate-result"

route_add oidc-backend.internal "$lkg_backend"
wait_public_ready
probe_direct oidc-backend-lkg.internal /health/ready

undeploy_if_present "$failed_app"; failed_deployed=false
undeploy_if_present "$candidate_app"; candidate_deployed=false
route_remove oidc-backend-lkg.internal
route_remove oidc-backend-candidate.internal
route_remove oidc-backend-failed.internal

psql_exec "BEGIN; DELETE FROM _migrations WHERE filename='V9000__phase8_additive_probe.sql'; DROP TABLE phase8_upgrade_probe; COMMIT;" >/dev/null
schema_probe=false
migrations_after=$(psql_exec "SELECT count(*) || '|' || count(DISTINCT filename) FROM _migrations")
[[ "$migrations_after" == "$migrations_before" ]] || {
  echo "Migration tracking did not return to baseline: before=$migrations_before after=$migrations_after" >&2
  exit 1
}

wait "$load_pid"
load_pid=
wait_public_ready
probe_direct oidc-backend.internal /health/ready
check_guest_clocks "$report_dir/clock-skew-after.txt"

python3 - "$report_dir/upgrade-load.json" "$report_dir/result.json" "$lkg_backend" "$candidate_app" \
  "$artifact_sha" "$migrations_before" "$migrations_after" "$failed_status" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    load=json.load(stream)
result={"status":"pass","lkg_backend":sys.argv[3],"candidate_backend":sys.argv[4],
        "artifact_sha256":sys.argv[5],"migrations_before":sys.argv[6],
        "migrations_after":sys.argv[7],"failed_candidate_http_status":sys.argv[8],
        "load":load}
with open(sys.argv[2], "w", encoding="utf-8") as stream:
    json.dump(result, stream, indent=2, sort_keys=True)
    stream.write("\n")
PY
chmod 600 "$report_dir/result.json"

trap - EXIT
echo "OIDC upgrade/rollback validation passed. Evidence: $report_dir/result.json"

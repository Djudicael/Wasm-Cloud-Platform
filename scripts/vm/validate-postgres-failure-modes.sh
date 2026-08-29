#!/usr/bin/env bash
set -euo pipefail

state_file=.vm-testbed-state.json
database=oidc
database_user=oidc
application_dir=../openid_connect_wasi
migrator=
backend_host=oidc-backend.internal
frontend_host=oidc.local

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --database) database=${2:?missing database}; shift 2 ;;
    --user) database_user=${2:?missing user}; shift 2 ;;
    --application-dir) application_dir=${2:?missing application directory}; shift 2 ;;
    --migrator) migrator=${2:?missing migrator path}; shift 2 ;;
    --backend-host) backend_host=${2:?missing backend host}; shift 2 ;;
    --frontend-host) frontend_host=${2:?missing frontend host}; shift 2 ;;
    -h|--help)
      cat <<'EOF'
Usage: PGPASSWORD=... validate-postgres-failure-modes.sh [OPTIONS]

Options:
  --state-file PATH       Testbed lifecycle state
  --database NAME         Database name (default: oidc)
  --user NAME             Database role (default: oidc)
  --application-dir PATH  OpenID Connect WASI Hub checkout
  --migrator PATH         Prebuilt oidc-migrate binary
  --backend-host HOST     HAProxy backend Host header
  --frontend-host HOST    HAProxy frontend Host header

This is a disruptive local-test runner. It validates invalid credentials,
connection exhaustion/recovery, query and lock deadlines, and migration
serialization. It never targets an unrecorded PostgreSQL endpoint.
EOF
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
[[ -n ${PGPASSWORD:-} ]] || { echo "Set PGPASSWORD without placing it on the command line." >&2; exit 1; }
for command_name in curl jq podman sha256sum; do
  command -v "$command_name" >/dev/null || { echo "Missing required command: $command_name" >&2; exit 1; }
done
[[ -f "$state_file" ]] || { echo "Missing topology state: $state_file" >&2; exit 1; }

state_file=$(realpath "$state_file")
service_state=${state_file}.services.json
[[ -f "$service_state" ]] || { echo "Missing companion service state: $service_state" >&2; exit 1; }
postgres_host=$(jq -er '.services[] | select(.kind == "postgresql") | .ip' "$state_file")
postgres_port=$(jq -er '.services[] | select(.kind == "postgresql") | .port' "$state_file")
front_door_bind=$(jq -er '.front_door.bind' "$service_state")
front_door_url=http://$front_door_bind

state_key=$(printf '%s' "$state_file" | sha256sum | cut -d' ' -f1)
short_key=${state_key:0:12}
runtime_root=${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-postgres-validation-$(id -u)
runtime_dir=$runtime_root/$state_key
connection_container=wcp-pg-connections-$short_key
table_lock_container=wcp-pg-table-lock-$short_key
migration_lock_container=wcp-pg-migration-lock-$short_key
validation_label=io.wasm-cloud-platform.postgres-validation

cleanup_container() {
  local name=$1 id label
  id=$(podman inspect --format '{{.Id}}' "$name" 2>/dev/null || true)
  label=$(podman inspect --format "{{index .Config.Labels \"$validation_label\"}}" "$name" 2>/dev/null || true)
  if [[ -n $id && $label == "$state_key" ]]; then
    podman rm -f "$id" >/dev/null
  fi
}

cleanup() {
  cleanup_container "$connection_container"
  cleanup_container "$table_lock_container"
  cleanup_container "$migration_lock_container"
  rm -rf -- "$runtime_dir"
}
trap cleanup EXIT

mkdir -p "$runtime_root"
chmod 700 "$runtime_root"
rm -rf -- "$runtime_dir"
mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"
for name in "$connection_container" "$table_lock_container" "$migration_lock_container"; do
  if podman container exists "$name"; then
    echo "Refusing to replace existing container $name." >&2
    exit 1
  fi
done

psql_query() {
  podman run --rm --network host --env PGPASSWORD \
    docker.io/library/postgres:17-alpine \
    psql -h "$postgres_host" -p "$postgres_port" -U "$database_user" \
    -d "$database" -v ON_ERROR_STOP=1 "$@"
}

http_status() {
  local host=$1 path=$2
  curl --silent --show-error --output /dev/null --write-out '%{http_code}' \
    --header "Host: $host" "$front_door_url$path"
}

wait_for_database_ready() {
  local status
  for _ in $(seq 1 45); do
    status=$(http_status "$backend_host" /health/ready || true)
    [[ $status == 200 ]] && return 0
    sleep 1
  done
  echo "Backend database readiness did not recover within 45s." >&2
  return 1
}

echo "[1/5] Invalid credentials fail promptly"
invalid_output=$(
  podman run --rm --network host --env PGPASSWORD=invalid-validation-only \
    docker.io/library/postgres:17-alpine \
    psql -h "$postgres_host" -p "$postgres_port" -U "$database_user" \
    -d "$database" -v ON_ERROR_STOP=1 -c 'SELECT 1' 2>&1 || true
)
grep -q 'password authentication failed' <<<"$invalid_output" || {
  echo "Invalid-credential check did not return the expected authentication failure." >&2
  exit 1
}
[[ $(http_status "$backend_host" /health/ready) == 200 ]]

echo "[2/5] Connection exhaustion is bounded and recovers"
max_connections=$(psql_query -Atqc 'SHOW max_connections')
reserved_connections=$(psql_query -Atqc 'SHOW superuser_reserved_connections')
clients=$((max_connections - reserved_connections - 1))
((clients > 0)) || { echo "Calculated invalid pgbench client count: $clients" >&2; exit 1; }
printf 'SELECT pg_sleep(20);\n' > "$runtime_dir/hold.sql"
podman run --detach --name "$connection_container" --network host \
  --label "$validation_label=$state_key" --env PGPASSWORD \
  --volume "$runtime_dir/hold.sql:/validation/hold.sql:ro,Z" \
  docker.io/library/postgres:17-alpine \
  pgbench -h "$postgres_host" -p "$postgres_port" -U "$database_user" \
  -d "$database" --client="$clients" --jobs=16 --transactions=1 \
  --file=/validation/hold.sql >/dev/null

observed_connections=0
for _ in $(seq 1 20); do
  observed_connections=$(psql_query -Atqc \
    "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database()" 2>/dev/null || true)
  [[ $observed_connections =~ ^[0-9]+$ ]] && ((observed_connections >= clients)) && break
  sleep 1
done
[[ $observed_connections =~ ^[0-9]+$ ]] && ((observed_connections >= clients)) || {
  echo "Failed to observe the expected connection pressure." >&2
  exit 1
}
backend_status=$(http_status "$backend_host" /health/ready || true)
frontend_status=$(http_status "$frontend_host" / || true)
[[ $backend_status == 503 ]] || { echo "Expected backend 503 during exhaustion, got $backend_status." >&2; exit 1; }
[[ $frontend_status == 200 ]] || { echo "Expected independent frontend 200, got $frontend_status." >&2; exit 1; }
podman wait "$connection_container" >/dev/null
cleanup_container "$connection_container"
wait_for_database_ready

echo "[3/5] Statement and application lock deadlines are bounded"
statement_dsn="postgresql://${database_user}:${PGPASSWORD}@${postgres_host}:${postgres_port}/${database}?sslmode=disable&connect_timeout=2&options=-c%20statement_timeout%3D2000"
start_seconds=$SECONDS
statement_output=$(podman run --rm --network host docker.io/library/postgres:17-alpine \
  psql "$statement_dsn" -v ON_ERROR_STOP=1 -c 'SELECT pg_sleep(10)' 2>&1 || true)
statement_elapsed=$((SECONDS - start_seconds))
grep -q 'canceling statement due to statement timeout' <<<"$statement_output" || {
  echo "Statement timeout was not enforced." >&2
  exit 1
}
((statement_elapsed <= 5)) || { echo "Statement timeout took ${statement_elapsed}s." >&2; exit 1; }

podman run --detach --name "$table_lock_container" --network host \
  --label "$validation_label=$state_key" --env PGPASSWORD \
  docker.io/library/postgres:17-alpine \
  psql -h "$postgres_host" -p "$postgres_port" -U "$database_user" -d "$database" \
  -v ON_ERROR_STOP=1 -c \
  'BEGIN; LOCK TABLE users IN ACCESS EXCLUSIVE MODE; SELECT pg_sleep(12); COMMIT;' >/dev/null
for _ in $(seq 1 20); do
  lock_count=$(psql_query -Atqc \
    "SELECT count(*) FROM pg_locks WHERE relation = 'users'::regclass AND mode = 'AccessExclusiveLock' AND granted" 2>/dev/null || true)
  [[ $lock_count == 1 ]] && break
  sleep 0.25
done
[[ ${lock_count:-0} == 1 ]] || { echo "Failed to acquire deterministic users-table lock." >&2; exit 1; }
login_payload='{"email":"admin@example.com","password":"DefinitelyWrong123","client_id":"admin-ui"}'
start_seconds=$SECONDS
login_status=$(curl --silent --show-error --output "$runtime_dir/login-response.json" \
  --write-out '%{http_code}' --max-time 8 --header "Host: $backend_host" \
  --header 'Content-Type: application/json' --data "$login_payload" \
  "$front_door_url/oidc/login" || true)
login_elapsed=$((SECONDS - start_seconds))
[[ $login_status == 500 ]] || { echo "Expected sanitized HTTP 500 on lock timeout, got $login_status." >&2; exit 1; }
grep -q '"error":"server_error"' "$runtime_dir/login-response.json"
((login_elapsed <= 5)) || { echo "Application lock timeout took ${login_elapsed}s." >&2; exit 1; }
[[ $(http_status "$backend_host" /health/ready) == 200 ]]
podman wait "$table_lock_container" >/dev/null
cleanup_container "$table_lock_container"

echo "[4/5] Migration lock serializes writers and recovers"
if [[ -z $migrator ]]; then
  application_dir=$(realpath "$application_dir")
  migrator=${CARGO_TARGET_DIR:-/tmp/openid-connect-wasi-target}/release/oidc-migrate
  if [[ ! -x $migrator ]]; then
    (cd "$application_dir" && cargo build --release -p oidc-migrate)
  fi
fi
migrator=$(realpath "$migrator")
[[ -x $migrator ]] || { echo "Missing executable migrator: $migrator" >&2; exit 1; }
database_url="postgresql://${database_user}:${PGPASSWORD}@${postgres_host}:${postgres_port}/${database}?sslmode=disable"
podman run --detach --name "$migration_lock_container" --network host \
  --label "$validation_label=$state_key" --env PGPASSWORD \
  docker.io/library/postgres:17-alpine \
  psql -h "$postgres_host" -p "$postgres_port" -U "$database_user" -d "$database" \
  -v ON_ERROR_STOP=1 -c \
  'SELECT pg_advisory_lock(734662019); SELECT pg_sleep(15); SELECT pg_advisory_unlock(734662019);' >/dev/null
for _ in $(seq 1 20); do
  advisory_count=$(psql_query -Atqc \
    "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND objid = 734662019 AND granted" 2>/dev/null || true)
  [[ $advisory_count == 1 ]] && break
  sleep 0.25
done
[[ ${advisory_count:-0} == 1 ]] || { echo "Failed to acquire deterministic migration lock." >&2; exit 1; }
start_seconds=$SECONDS
set +e
migration_output=$(cd "$application_dir" && OIDC_DATABASE_URL="$database_url" "$migrator" 2>&1)
migration_exit=$?
set -e
migration_elapsed=$((SECONDS - start_seconds))
((migration_exit != 0)) || { echo "Concurrent migrator unexpectedly bypassed the advisory lock." >&2; exit 1; }
grep -q 'failed to acquire the migration lock within 10s' <<<"$migration_output"
((migration_elapsed >= 9 && migration_elapsed <= 13)) || {
  echo "Migration lock timeout took ${migration_elapsed}s; expected about 10s." >&2
  exit 1
}
podman wait "$migration_lock_container" >/dev/null
cleanup_container "$migration_lock_container"
(cd "$application_dir" && OIDC_DATABASE_URL="$database_url" "$migrator")

echo "[5/5] Final consistency and recovery gates"
migration_counts=$(psql_query -Atqc \
  'SELECT count(*) || '"'|'"' || count(DISTINCT filename) FROM _migrations')
IFS='|' read -r migration_total migration_distinct <<<"$migration_counts"
[[ $migration_total == "$migration_distinct" ]] || {
  echo "Duplicate migration tracking rows found: total=$migration_total distinct=$migration_distinct" >&2
  exit 1
}
wait_for_database_ready
[[ $(http_status "$frontend_host" /) == 200 ]]

echo "PostgreSQL failure-mode validation passed."
echo "Endpoint: $postgres_host:$postgres_port (resolved from $state_file)"
echo "Connection pressure: $observed_connections sessions; configured max/reserved: $max_connections/$reserved_connections"
echo "Migration tracking rows: $migration_total unique filenames"

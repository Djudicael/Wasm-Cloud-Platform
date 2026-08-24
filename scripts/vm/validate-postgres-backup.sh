#!/usr/bin/env bash
set -euo pipefail

state_file=.vm-testbed-state.json
database=oidc
user=oidc

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --database) database=${2:?missing database}; shift 2 ;;
    --user) user=${2:?missing user}; shift 2 ;;
    -h|--help)
      echo "Usage: PGPASSWORD=... validate-postgres-backup.sh [--state-file PATH] [--database NAME] [--user NAME]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
[[ -n ${PGPASSWORD:-} ]] || { echo "Set PGPASSWORD without placing it on the command line." >&2; exit 1; }
for command_name in jq podman sha256sum; do
  command -v "$command_name" >/dev/null || { echo "Missing required command: $command_name" >&2; exit 1; }
done
[[ -f "$state_file" ]] || { echo "Missing topology state: $state_file" >&2; exit 1; }

state_file=$(realpath "$state_file")
postgres_host=$(jq -er '.services[] | select(.kind == "postgresql") | .ip' "$state_file")
state_key=$(printf '%s' "$state_file" | sha256sum | cut -d' ' -f1)
short_key=${state_key:0:12}
runtime_root=${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-backup-validation-$(id -u)
runtime_dir=$runtime_root/$state_key
container_name=wcp-backup-restore-$short_key
backup_file=$runtime_dir/oidc.dump
container_started=false

cleanup() {
  if [[ $container_started == true ]]; then
    recorded_id=$(podman inspect --format '{{.Id}}' "$container_name" 2>/dev/null || true)
    recorded_label=$(podman inspect --format '{{index .Config.Labels "io.wasm-cloud-platform.validation"}}' "$container_name" 2>/dev/null || true)
    if [[ -n $recorded_id && $recorded_label == "$state_key" ]]; then
      podman rm -f "$recorded_id" >/dev/null
    fi
  fi
  rm -rf -- "$runtime_dir"
}
trap cleanup EXIT

mkdir -p "$runtime_root"
chmod 700 "$runtime_root"
rm -rf -- "$runtime_dir"
mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir"

if podman container exists "$container_name"; then
  echo "Refusing to replace existing container $container_name." >&2
  exit 1
fi

source_tables=$(podman run --rm --network host --env PGPASSWORD \
  docker.io/library/postgres:17-alpine \
  psql -h "$postgres_host" -U "$user" -d "$database" -Atqc \
  "SELECT count(*) FROM pg_tables WHERE schemaname = 'public'" )
source_migrations=$(podman run --rm --network host --env PGPASSWORD \
  docker.io/library/postgres:17-alpine \
  psql -h "$postgres_host" -U "$user" -d "$database" -Atqc \
  "SELECT count(*) FROM public._migrations" )

podman run --rm --network host --env PGPASSWORD \
  --volume "$runtime_dir:/backup:Z" \
  docker.io/library/postgres:17-alpine \
  pg_dump -h "$postgres_host" -U "$user" -d "$database" \
  --format=custom --no-owner --no-privileges --file=/backup/oidc.dump
chmod 600 "$backup_file"

podman run --detach --name "$container_name" \
  --label "io.wasm-cloud-platform.validation=$state_key" \
  --env POSTGRES_PASSWORD=local-restore-validation-only \
  --volume "$runtime_dir:/backup:ro,Z" \
  docker.io/library/postgres:17-alpine >/dev/null
container_started=true

for _ in $(seq 1 30); do
  if podman exec "$container_name" pg_isready -U postgres >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
podman exec "$container_name" pg_isready -U postgres >/dev/null
podman exec "$container_name" createdb -U postgres restored_oidc
podman exec "$container_name" pg_restore -U postgres -d restored_oidc \
  --exit-on-error --no-owner --no-privileges /backup/oidc.dump

restored_tables=$(podman exec "$container_name" psql -U postgres -d restored_oidc -Atqc \
  "SELECT count(*) FROM pg_tables WHERE schemaname = 'public'")
restored_migrations=$(podman exec "$container_name" psql -U postgres -d restored_oidc -Atqc \
  "SELECT count(*) FROM public._migrations")

[[ $restored_tables == "$source_tables" ]] || {
  echo "Restore table-count mismatch: source=$source_tables restored=$restored_tables" >&2
  exit 1
}
[[ $restored_migrations == "$source_migrations" ]] || {
  echo "Restore migration-count mismatch: source=$source_migrations restored=$restored_migrations" >&2
  exit 1
}

backup_sha=$(sha256sum "$backup_file" | cut -d' ' -f1)
echo "PostgreSQL logical backup restore validation passed."
echo "Source/restored public tables: $source_tables"
echo "Source/restored migration rows: $source_migrations"
echo "Backup SHA-256: $backup_sha"

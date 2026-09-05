#!/usr/bin/env bash
# Validate PostgreSQL clock discipline and suspend/resume recovery in a recorded testbed.

set -euo pipefail

state_file=.vm-testbed-state.json
output_file=
pause_seconds=90
max_skew_seconds=5

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --output) output_file=${2:?missing output path}; shift 2 ;;
    --pause-seconds) pause_seconds=${2:?missing pause duration}; shift 2 ;;
    --max-skew-seconds) max_skew_seconds=${2:?missing skew threshold}; shift 2 ;;
    -h|--help)
      echo "Usage: validate-postgres-clock.sh [--state-file PATH] [--output PATH] [--pause-seconds 90] [--max-skew-seconds 5]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
[[ "$pause_seconds" =~ ^[0-9]+$ ]] && ((pause_seconds >= 75)) || {
  echo "Pause duration must be at least 75 seconds to exercise long-suspend clock recovery." >&2
  exit 2
}
[[ "$max_skew_seconds" =~ ^[0-9]+([.][0-9]+)?$ ]] || {
  echo "Maximum skew must be a non-negative number." >&2
  exit 2
}
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"
for command_name in curl debugfs jq podman python3; do
  command -v "$command_name" >/dev/null || { echo "$command_name is required." >&2; exit 1; }
done
state_file=$(realpath "$state_file")
services_file="${state_file}.services.json"
[[ -f "$state_file" && -f "$services_file" ]] || { echo "Missing topology or service state." >&2; exit 1; }

mapfile -t postgres_state < <(jq -er '.services[] | select(.id == "oidc-postgres" and .kind == "postgresql") | .pid,.ip' "$state_file")
(( ${#postgres_state[@]} == 2 )) || { echo "Expected one recorded oidc-postgres service." >&2; exit 1; }
postgres_pid=${postgres_state[0]}
postgres_ip=${postgres_state[1]}
postgres_rootfs=$repo_root/assets/postgres-rootfs.ext4
[[ -f "$postgres_rootfs" ]] || { echo "Missing canonical PostgreSQL rootfs." >&2; exit 1; }
[[ -r "/proc/$postgres_pid/cmdline" ]] || { echo "Recorded PostgreSQL VM PID is not alive: $postgres_pid" >&2; exit 1; }
cmdline=$(tr '\0' ' ' < "/proc/$postgres_pid/cmdline")
[[ "$cmdline" == *firecracker* && "$cmdline" == *"vm-testbed-oidc-postgres"* ]] || {
  echo "Recorded PID does not match the oidc-postgres Firecracker identity." >&2
  exit 1
}
schema=$(debugfs -R 'cat /etc/postgresql-image-schema-version' "$postgres_rootfs" 2>/dev/null || true)
[[ "$schema" == 5 ]] || { echo "Expected PostgreSQL image schema 5, found: $schema" >&2; exit 1; }

prometheus_url=$(jq -er '.observability.prometheus' "$services_file")
[[ "$prometheus_url" == http://127.0.0.1:9095 ]] || { echo "Unexpected Prometheus endpoint." >&2; exit 1; }
serial_log=/tmp/vm-testbed-oidc-postgres/serial.log
[[ -f "$serial_log" ]] || { echo "Missing PostgreSQL serial log: $serial_log" >&2; exit 1; }

source_psql() {
  podman run --rm --network host --env PGPASSWORD=oidc-local-test \
    docker.io/library/postgres:17-alpine psql -v ON_ERROR_STOP=1 \
    -h "$postgres_ip" -U oidc -d oidc -Atqc "$1"
}

clock_skew() {
  local database_epoch host_epoch
  database_epoch=$(source_psql 'SELECT EXTRACT(EPOCH FROM clock_timestamp())')
  host_epoch=$(date +%s.%N)
  python3 - "$database_epoch" "$host_epoch" <<'PY'
import sys
print(f"{abs(float(sys.argv[1]) - float(sys.argv[2])):.6f}")
PY
}

assert_skew() {
  python3 - "$1" "$max_skew_seconds" <<'PY'
import sys
observed, maximum = map(float, sys.argv[1:])
if observed > maximum:
    raise SystemExit(f"database clock skew {observed:.6f}s exceeds {maximum:.6f}s")
PY
}

alert_active() {
  local alert_name=$1
  curl -fsS "$prometheus_url/api/v1/alerts" | jq -e --arg name "$alert_name" \
    'any(.data.alerts[]?; .labels.alertname == $name and .state == "firing")' >/dev/null
}

wait_database() {
  local deadline=$((SECONDS + 90))
  while ((SECONDS < deadline)); do
    source_psql 'SELECT 1' >/dev/null 2>&1 && return 0
    sleep 1
  done
  echo "PostgreSQL did not recover after resume." >&2
  return 1
}

wait_alert() {
  local name=$1 expected=$2 deadline=$((SECONDS + 75))
  while ((SECONDS < deadline)); do
    if alert_active "$name"; then
      [[ "$expected" == firing ]] && return 0
    else
      [[ "$expected" == resolved ]] && return 0
    fi
    sleep 1
  done
  echo "$name did not become $expected." >&2
  return 1
}

grep -q 'chrony: initial tracking state' "$serial_log" || { echo "Chrony startup evidence is absent." >&2; exit 1; }
grep -q 'Reference ID' "$serial_log" || { echo "Chrony tracking output is absent." >&2; exit 1; }
grep -Eq 'Leap status[[:space:]]*:[[:space:]]*Normal' "$serial_log" || { echo "Chrony has not reported a synchronized clock." >&2; exit 1; }
tracking_samples_before=$(grep -c 'chrony: periodic tracking state' "$serial_log" || true)
before_skew=$(clock_skew)
assert_skew "$before_skew"
prometheus_skew=$(curl -fsS --get --data-urlencode 'query=abs(time() - pg_clock_epoch_seconds)' \
  "$prometheus_url/api/v1/query" | jq -er '.data.result | if length == 1 then .[0].value[1] else error("clock metric missing") end')
assert_skew "$prometheus_skew"

if [[ -n ${WSL_DISTRO_NAME:-} ]] && command -v wsl.exe >/dev/null; then
  privileged_kill() { wsl.exe -u root -- kill "$@"; }
else
  sudo -v
  privileged_kill() { sudo kill "$@"; }
fi
paused=true
cleanup() {
  if [[ "$paused" == true && -d "/proc/$postgres_pid" ]]; then
    privileged_kill -CONT "$postgres_pid" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT
privileged_kill -STOP "$postgres_pid"
sleep "$pause_seconds"
wait_alert PostgreSQLClockMetricMissing firing
wait_alert PostgreSQLTimeSourceUnavailable firing
privileged_kill -CONT "$postgres_pid"
paused=false
wait_database
wait_alert PostgreSQLClockMetricMissing resolved
wait_alert PostgreSQLTimeSourceUnavailable resolved

deadline=$((SECONDS + 45))
while ((SECONDS < deadline)); do
  tracking_samples_after=$(grep -c 'chrony: periodic tracking state' "$serial_log" || true)
  ((tracking_samples_after > tracking_samples_before)) && break
  sleep 1
done
((tracking_samples_after > tracking_samples_before)) || {
  echo "Chrony did not emit a fresh tracking sample after resume." >&2
  exit 1
}

deadline=$((SECONDS + 75))
while ((SECONDS < deadline)); do
  after_skew=$(clock_skew 2>/dev/null || true)
  if [[ -n "$after_skew" ]] && assert_skew "$after_skew" 2>/dev/null; then
    break
  fi
  sleep 1
done
[[ -n ${after_skew:-} ]] || { echo "No post-resume clock reading." >&2; exit 1; }
assert_skew "$after_skew"
alert_active PostgreSQLClockSkewHigh && { echo "PostgreSQLClockSkewHigh remains firing after recovery." >&2; exit 1; }

result=$(jq -n \
  --arg result pass --arg schema "$schema" \
  --argjson pid "$postgres_pid" --argjson pause_seconds "$pause_seconds" \
  --argjson before_skew_seconds "$before_skew" \
  --argjson prometheus_skew_seconds "$prometheus_skew" \
  --argjson after_skew_seconds "$after_skew" \
  --argjson chrony_tracking_samples_before "$tracking_samples_before" \
  --argjson chrony_tracking_samples_after "$tracking_samples_after" \
  '{result:$result,image_schema:($schema|tonumber),postgres_vm_pid:$pid,pause_seconds:$pause_seconds,before_skew_seconds:$before_skew_seconds,prometheus_skew_seconds:$prometheus_skew_seconds,after_skew_seconds:$after_skew_seconds,clock_metric_missing_alert:"fired-and-resolved",clock_skew_alert:"promtool-tested-and-not-firing-after-recovery",time_source_alert:"fired-and-resolved",chrony_serial_evidence:true,chrony_tracking_samples_before:$chrony_tracking_samples_before,chrony_tracking_samples_after:$chrony_tracking_samples_after}')
if [[ -n "$output_file" ]]; then
  output_file=$(realpath -m "$output_file")
  mkdir -p "$(dirname "$output_file")"
  umask 077
  printf '%s\n' "$result" > "$output_file"
fi
printf '%s\n' "$result" | jq .

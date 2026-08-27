#!/usr/bin/env bash
set -euo pipefail

state_file=.vm-testbed-state.json
while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    -h|--help)
      echo "Usage: reload-haproxy-front-door.sh [--state-file PATH]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
cd "$repo_root"

services_file=${state_file}.services.json
[[ -f "$state_file" ]] || { echo "Missing topology state: $state_file" >&2; exit 1; }
[[ -f "$services_file" ]] || { echo "Missing service state: $services_file" >&2; exit 1; }
for command_name in haproxy jq readlink setsid; do
  command -v "$command_name" >/dev/null || { echo "Missing required command: $command_name" >&2; exit 1; }
done

expected_config=$(readlink -f "${state_file}.haproxy.cfg")
config=$(jq -er '.front_door.config' "$services_file")
config=$(readlink -f "$config")
[[ "$config" == "$expected_config" ]] || {
  echo "Refusing unexpected HAProxy config path: $config" >&2
  exit 1
}
[[ -f "$config" ]] || { echo "Missing HAProxy config: $config" >&2; exit 1; }

old_pid=$(jq -er '.front_door.pid' "$services_file")
[[ "$old_pid" =~ ^[1-9][0-9]*$ ]] || { echo "Invalid recorded HAProxy PID." >&2; exit 1; }
if kill -0 "$old_pid" 2>/dev/null; then
  executable=$(readlink -f "/proc/${old_pid}/exe" 2>/dev/null || true)
  [[ ${executable##*/} == haproxy ]] || {
    echo "Recorded PID $old_pid is not HAProxy; refusing to signal it." >&2
    exit 1
  }
fi

haproxy -c -f "$config"
if kill -0 "$old_pid" 2>/dev/null; then
  kill "$old_pid"
  for _ in {1..50}; do
    kill -0 "$old_pid" 2>/dev/null || break
    sleep 0.1
  done
  kill -0 "$old_pid" 2>/dev/null && {
    echo "Recorded HAProxy PID $old_pid did not stop." >&2
    exit 1
  }
fi

log=$(jq -er '.front_door.log' "$services_file")
setsid haproxy -db -f "$config" </dev/null >>"$log" 2>&1 &
new_pid=$!
sleep 1
kill -0 "$new_pid" 2>/dev/null || {
  echo "Replacement HAProxy failed to start; inspect $log" >&2
  exit 1
}

temporary=${services_file}.tmp
jq --argjson pid "$new_pid" \
  '.front_door.pid = $pid | .front_door.metrics = "http://127.0.0.1:8405/metrics"' \
  "$services_file" >"$temporary"
mv -- "$temporary" "$services_file"

echo "Reloaded HAProxy PID $old_pid -> $new_pid; metrics: http://127.0.0.1:8405/metrics"

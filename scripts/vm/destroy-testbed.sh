#!/usr/bin/env bash
set -euo pipefail

state_file=.vm-testbed-state.json
while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    -h|--help) echo "Usage: destroy-testbed.sh [--state-file PATH]"; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"

services_file="${state_file}.services.json"
if [[ -f "$services_file" ]]; then
  command -v python3 >/dev/null || { echo "python3 is required to read front-door state." >&2; exit 1; }
  mapfile -t front_door_state < <(python3 - "$services_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    front_door = (json.load(stream).get("front_door") or {})
print(front_door.get("type", ""))
print(front_door.get("pid", ""))
print(front_door.get("config", ""))
print(front_door.get("log", ""))
PY
  )
  front_door_type=${front_door_state[0]:-}
  front_door_pid=${front_door_state[1]:-}
  front_door_config=${front_door_state[2]:-}
  front_door_log=${front_door_state[3]:-}
  [[ "$front_door_type" == haproxy && "$front_door_pid" =~ ^[1-9][0-9]*$ && -n "$front_door_config" ]] || {
    echo "Invalid front-door lifecycle state: $services_file" >&2
    exit 1
  }
  expected_front_door_config=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "${state_file}.haproxy.cfg")
  expected_front_door_log=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "${state_file}.haproxy.log")
  [[ "$front_door_config" == "$expected_front_door_config" && "$front_door_log" == "$expected_front_door_log" ]] || {
    echo "Front-door paths in $services_file do not match the selected state file; refusing cleanup." >&2
    exit 1
  }
  if kill -0 "$front_door_pid" 2>/dev/null; then
    process_args=$(tr '\0' ' ' < "/proc/$front_door_pid/cmdline")
    [[ "$process_args" == *haproxy* && "$process_args" == *"$front_door_config"* ]] || {
      echo "PID $front_door_pid no longer matches the recorded HAProxy process; refusing to kill it." >&2
      exit 1
    }
    kill "$front_door_pid"
    for _ in {1..50}; do
      kill -0 "$front_door_pid" 2>/dev/null || break
      sleep 0.1
    done
    kill -0 "$front_door_pid" 2>/dev/null && {
      echo "Recorded HAProxy process $front_door_pid did not stop; lifecycle state was retained." >&2
      exit 1
    }
  fi
  rm -f -- "$expected_front_door_config" "$expected_front_door_log" "$services_file"
  echo "Stopped the recorded HAProxy front door."
fi

if [[ ! -f "$state_file" ]]; then
  echo "No VM state file at $state_file; the testbed is already down."
  exit 0
fi

command -v sudo >/dev/null || { echo "sudo is required for TAP/bridge cleanup." >&2; exit 1; }
sudo -v
target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
CARGO_TARGET_DIR="$target_dir" cargo build -p vm-testbed --bin vm-testbed-cli
cli="$target_dir/debug/vm-testbed-cli"
sudo -E "$cli" down --state-file "$state_file"

if [[ -e "$state_file" ]]; then
  echo "Teardown returned but the state file remains: $state_file" >&2
  exit 1
fi
echo "Testbed destroyed and state removed: $state_file"

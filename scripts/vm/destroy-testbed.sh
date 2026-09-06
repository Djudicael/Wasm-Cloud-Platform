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
  state_absolute=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$state_file")
  observability_state_key=$(printf '%s' "$state_absolute" | sha256sum | cut -d' ' -f1)
  observability_root="${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-observability-$(id -u)"
  expected_observability_dir="$observability_root/$observability_state_key"
  mapfile -t observability_state < <(python3 - "$services_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    observability = json.load(stream).get("observability") or {}
print(observability.get("state_key", ""))
print(observability.get("runtime_dir", ""))
for container in observability.get("containers", []):
    print(f'{container.get("name", "")}\t{container.get("id", "")}')
PY
  )
  recorded_observability_key=${observability_state[0]:-}
  recorded_observability_dir=${observability_state[1]:-}
  if [[ -n "$recorded_observability_key" || -n "$recorded_observability_dir" ]]; then
    command -v podman >/dev/null || { echo "podman is required to stop recorded observability services." >&2; exit 1; }
    [[ "$recorded_observability_key" == "$observability_state_key" \
      && "$recorded_observability_dir" == "$expected_observability_dir" ]] || {
      echo "Observability state does not match the selected state file; refusing cleanup." >&2
      exit 1
    }
    for record in "${observability_state[@]:2}"; do
      IFS=$'\t' read -r container_name container_id <<< "$record"
      [[ -n "$container_name" && "$container_id" =~ ^[a-f0-9]{64}$ ]] || {
        echo "Invalid observability container record; refusing cleanup." >&2
        exit 1
      }
      if podman container exists "$container_id"; then
        inspected_name=$(podman inspect --format '{{.Name}}' "$container_id")
        inspected_label=$(podman inspect --format '{{index .Config.Labels "io.wasm-cloud-platform.state"}}' "$container_id")
        [[ "$inspected_name" == "$container_name" && "$inspected_label" == "$observability_state_key" ]] || {
          echo "Container $container_id does not match recorded observability state; refusing cleanup." >&2
          exit 1
        }
        podman rm -f "$container_id" >/dev/null
      fi
    done
    if [[ -d "$recorded_observability_dir" ]]; then
      [[ "$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$recorded_observability_dir")" == "$expected_observability_dir" ]] || {
        echo "Observability runtime directory mismatch; refusing cleanup." >&2
        exit 1
      }
      rm -rf -- "$expected_observability_dir"
      rmdir --ignore-fail-on-non-empty "$observability_root" 2>/dev/null || true
    fi
    python3 - "$services_file" <<'PY'
import json, os, sys, tempfile
path = sys.argv[1]
with open(path, encoding="utf-8") as stream:
    state = json.load(stream)
state.pop("observability", None)
directory = os.path.dirname(os.path.abspath(path))
fd, temporary = tempfile.mkstemp(prefix=".services-", dir=directory, text=True)
try:
    with os.fdopen(fd, "w", encoding="utf-8") as stream:
        json.dump(state, stream, indent=2)
        stream.write("\n")
    os.replace(temporary, path)
except BaseException:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    raise
PY
    echo "Stopped the recorded local observability services."
  fi
  mapfile -t vault_state < <(python3 - "$services_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    vault = json.load(stream).get("vault") or {}
print(vault.get("state_key", ""))
print(vault.get("runtime_dir", ""))
PY
  )
  recorded_vault_key=${vault_state[0]:-}
  recorded_vault_dir=${vault_state[1]:-}
  if [[ -n "$recorded_vault_key" || -n "$recorded_vault_dir" ]]; then
    vault_root="${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-vault-$(id -u)"
    expected_vault_dir="$vault_root/$observability_state_key"
    [[ "$recorded_vault_key" == "$observability_state_key" \
      && "$recorded_vault_dir" == "$expected_vault_dir" ]] || {
      echo "Vault runtime state does not match the selected state file; refusing cleanup." >&2
      exit 1
    }
    if [[ -d "$expected_vault_dir" ]]; then
      rm -rf -- "$expected_vault_dir"
      rmdir --ignore-fail-on-non-empty "$vault_root" 2>/dev/null || true
    fi
    python3 - "$services_file" <<'PY'
import json, os, sys, tempfile
path = sys.argv[1]
with open(path, encoding="utf-8") as stream:
    state = json.load(stream)
state.pop("vault", None)
directory = os.path.dirname(os.path.abspath(path))
fd, temporary = tempfile.mkstemp(prefix=".services-", dir=directory, text=True)
try:
    with os.fdopen(fd, "w", encoding="utf-8") as stream:
        json.dump(state, stream, indent=2)
        stream.write("\n")
    os.replace(temporary, path)
except BaseException:
    try:
        os.unlink(temporary)
    except FileNotFoundError:
        pass
    raise
PY
    echo "Removed the recorded local Vault credentials."
  fi
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

state_file=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$state_file")
oidc_secret_root="${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-oidc-secrets-$(id -u)"
oidc_state_key=$(printf '%s' "$state_file" | sha256sum | cut -d' ' -f1)
oidc_secret_dir="$oidc_secret_root/$oidc_state_key"

target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
CARGO_TARGET_DIR="$target_dir" cargo build -p vm-testbed --bin vm-testbed-cli
cli="$target_dir/debug/vm-testbed-cli"
if [[ -n ${WSL_DISTRO_NAME:-} ]] && command -v wsl.exe >/dev/null; then
  # The Windows user owns the WSL distro and can invoke its root account without
  # placing a sudo password in automation or requiring a terminal-bound ticket.
  wsl.exe -u root -- "$cli" down --state-file "$state_file"
else
  command -v sudo >/dev/null || { echo "sudo is required for TAP/bridge cleanup." >&2; exit 1; }
  sudo -v
  sudo -E "$cli" down --state-file "$state_file"
fi

if [[ -e "$state_file" ]]; then
  echo "Teardown returned but the state file remains: $state_file" >&2
  exit 1
fi
if [[ -d "$oidc_secret_dir" ]]; then
  expected_secret_dir="$oidc_secret_root/$oidc_state_key"
  [[ "$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$oidc_secret_dir")" == "$expected_secret_dir" ]] || {
    echo "OIDC secret directory does not match the selected state file; refusing cleanup." >&2
    exit 1
  }
  rm -rf -- "$expected_secret_dir"
  rmdir --ignore-fail-on-non-empty "$oidc_secret_root" 2>/dev/null || true
  echo "Removed the local OIDC test credentials and signing keys."
fi
echo "Testbed destroyed and state removed: $state_file"

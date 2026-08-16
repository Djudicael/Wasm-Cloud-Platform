#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: deploy-test-application.sh [options]
  --app NAME             application name (default: hello-axum)
  --version VERSION      application version (default: v1)
  --namespace NAME       namespace (default: default)
  --manifest PATH        Cargo manifest to build for wasm32-wasip2
  --wasm PATH            prebuilt Wasm component (skips the app build)
  --route-host HOST      route and HTTP Host header (default: hello.local)
  --state-file PATH      testbed state (default: .vm-testbed-state.json)
  --timeout SECONDS      verification timeout (default: 90)
EOF
}

app=hello-axum
version=v1
namespace=default
manifest=
wasm=
route_host=hello.local
state_file=.vm-testbed-state.json
timeout=90

while (($#)); do
  case "$1" in
    --app) app=${2:?missing app}; shift 2 ;;
    --version) version=${2:?missing version}; shift 2 ;;
    --namespace) namespace=${2:?missing namespace}; shift 2 ;;
    --manifest) manifest=${2:?missing manifest}; shift 2 ;;
    --wasm) wasm=${2:?missing Wasm path}; shift 2 ;;
    --route-host) route_host=${2:?missing host}; shift 2 ;;
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --timeout) timeout=${2:?missing timeout}; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"
[[ -f "$state_file" ]] || { echo "Missing state file: $state_file" >&2; exit 1; }
command -v curl >/dev/null || { echo "curl is required for verification." >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 is required to read testbed state." >&2; exit 1; }
sudo -v

target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
CARGO_TARGET_DIR="$target_dir" cargo build -p vm-testbed --bin vm-testbed-cli
cli="$target_dir/debug/vm-testbed-cli"
sudo -E "$cli" status --state-file "$state_file"

if [[ -z "$wasm" ]]; then
  [[ -n "$manifest" ]] || manifest="apps/$app/Cargo.toml"
  [[ -f "$manifest" ]] || { echo "Missing manifest: $manifest" >&2; exit 1; }
  CARGO_TARGET_DIR="$target_dir" cargo build --manifest-path "$manifest" --target wasm32-wasip2 --release
  artifact_name=${app//-/_}.wasm
  wasm="$target_dir/wasm32-wasip2/release/$artifact_name"
fi
[[ -f "$wasm" ]] || { echo "Missing Wasm artifact: $wasm" >&2; exit 1; }

sudo -E "$cli" deploy-app \
  --state-file "$state_file" \
  --app "$app" \
  --version "$version" \
  --namespace "$namespace" \
  --wasm "$wasm" \
  --route-host "$route_host"

proxy_addr=$(sudo python3 - "$state_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    state = json.load(stream)
nodes = state.get("nodes", [])
if not nodes:
    raise SystemExit("state contains no platform nodes")
print(nodes[0]["proxy_addr"])
PY
)

verification_target="node proxy"
services_file="${state_file}.services.json"
if [[ -f "$services_file" ]]; then
  mapfile -t front_door_state < <(python3 - "$services_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    services = json.load(stream)
front_door = services.get("front_door") or {}
if front_door.get("type") == "haproxy":
    print(front_door["bind"])
    print(front_door["pid"])
PY
  )
  front_door_addr=${front_door_state[0]:-}
  front_door_pid=${front_door_state[1]:-}
  if [[ -n "$front_door_addr" ]]; then
    [[ "$front_door_pid" =~ ^[1-9][0-9]*$ ]] && kill -0 "$front_door_pid" 2>/dev/null || {
      echo "The recorded HAProxy front door is not running; inspect or destroy the testbed before deploying." >&2
      exit 1
    }
    proxy_addr=$front_door_addr
    verification_target="HAProxy front door"
  fi
fi

deadline=$((SECONDS + timeout))
response_file=$(mktemp)
trap 'rm -f "$response_file"' EXIT
while ((SECONDS < deadline)); do
  status=$(curl --silent --show-error --output "$response_file" --write-out '%{http_code}' \
    --max-time 5 --header "Host: $route_host" "http://$proxy_addr/" || true)
  if [[ "$status" =~ ^2[0-9][0-9]$ ]]; then
    echo "Verified $namespace:$app:$version through $verification_target at http://$proxy_addr/ (Host: $route_host)"
    cat "$response_file"
    printf '\n'
    exit 0
  fi
  sleep 2
done

echo "Deployment verification timed out after ${timeout}s (last HTTP status: ${status:-none})." >&2
exit 1

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
  --route-path PATH      route prefix; repeat for multiple prefixes (default: /)
  --health-path PATH     runtime health path, or none (default: /health)
  --env KEY=VALUE        application environment variable; repeat as needed
  --verify-path PATH     HTTP path used for verification (default: /)
  --verify-contains TEXT require TEXT in the verification response body
  --state-file PATH      testbed state (default: .vm-testbed-state.json)
  --auth-token TOKEN     local admin/artifact token (default: WASM_CTL_AUTH_TOKEN or testbed token)
  --timeout SECONDS      verification timeout (default: 90)
  --fuel UNITS           per-request Wasm fuel quota (default: 500000000)
  --memory-mb MIB        per-instance linear-memory limit (default: 128)
  --rate-limit-rps N     sustained per-node application request limit
  --rate-limit-burst N   token-bucket burst capacity (requires rate-limit-rps)
  --rate-limit-per-ip N  per-client-IP request limit (requires rate-limit-rps)
  --target-node NAME     request deployment on a specific platform node
  --allowed-cidr CIDR    allowed outbound CIDR; repeat as needed
  --denied-cidr CIDR     denied outbound CIDR; repeat as needed
  --allowed-filesystem-path PATH  writable preopen; repeat as needed
  --verify-direct-node   bypass a topology-specific HAProxy configuration
  --max-outbound-connections N  simultaneous outbound TCP limit (default: 100)
  --max-open-fds N        simultaneous WASI file-descriptor limit (default: 256)
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
fuel=500000000
memory_mb=128
rate_limit_rps=
rate_limit_burst=
rate_limit_per_ip=
target_node=
verify_direct_node=false
max_outbound_connections=100
max_open_fds=256
auth_token=${WASM_CTL_AUTH_TOKEN:-local-test-write-token-change-me}
route_paths=()
allowed_cidrs=()
denied_cidrs=()
allowed_filesystem_paths=()
health_path=/health
env_vars=()
verify_path=/
verify_contains=

while (($#)); do
  case "$1" in
    --app) app=${2:?missing app}; shift 2 ;;
    --version) version=${2:?missing version}; shift 2 ;;
    --namespace) namespace=${2:?missing namespace}; shift 2 ;;
    --manifest) manifest=${2:?missing manifest}; shift 2 ;;
    --wasm) wasm=${2:?missing Wasm path}; shift 2 ;;
    --route-host) route_host=${2:?missing host}; shift 2 ;;
    --route-path) route_paths+=("${2:?missing route path}"); shift 2 ;;
    --health-path) health_path=${2:?missing health path}; shift 2 ;;
    --env) env_vars+=("${2:?missing environment variable}"); shift 2 ;;
    --verify-path) verify_path=${2:?missing verification path}; shift 2 ;;
    --verify-contains) verify_contains=${2:?missing expected text}; shift 2 ;;
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --auth-token) auth_token=${2:?missing auth token}; shift 2 ;;
    --timeout) timeout=${2:?missing timeout}; shift 2 ;;
    --fuel) fuel=${2:?missing fuel quota}; shift 2 ;;
    --memory-mb) memory_mb=${2:?missing memory limit}; shift 2 ;;
    --rate-limit-rps) rate_limit_rps=${2:?missing rate limit}; shift 2 ;;
    --rate-limit-burst) rate_limit_burst=${2:?missing burst limit}; shift 2 ;;
    --rate-limit-per-ip) rate_limit_per_ip=${2:?missing per-IP limit}; shift 2 ;;
    --target-node) target_node=${2:?missing target node}; shift 2 ;;
    --allowed-cidr) allowed_cidrs+=("${2:?missing allowed CIDR}"); shift 2 ;;
    --denied-cidr) denied_cidrs+=("${2:?missing denied CIDR}"); shift 2 ;;
    --allowed-filesystem-path) allowed_filesystem_paths+=("${2:?missing filesystem path}"); shift 2 ;;
    --verify-direct-node) verify_direct_node=true; shift ;;
    --max-outbound-connections) max_outbound_connections=${2:?missing limit}; shift 2 ;;
    --max-open-fds) max_open_fds=${2:?missing limit}; shift 2 ;;
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
target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
CARGO_TARGET_DIR="$target_dir" cargo build -p vm-testbed --bin vm-testbed-cli
cli="$target_dir/debug/vm-testbed-cli"
"$cli" status --state-file "$state_file"

if [[ -z "$wasm" ]]; then
  [[ -n "$manifest" ]] || manifest="apps/$app/Cargo.toml"
  [[ -f "$manifest" ]] || { echo "Missing manifest: $manifest" >&2; exit 1; }
  CARGO_TARGET_DIR="$target_dir" cargo build --manifest-path "$manifest" --target wasm32-wasip2 --release
  manifest_abs=$(realpath "$manifest")
  artifact_name=$(CARGO_TARGET_DIR="$target_dir" cargo metadata \
    --manifest-path "$manifest_abs" --no-deps --format-version 1 | python3 -c '
import json, sys
metadata = json.load(sys.stdin)
package = next(
    package for package in metadata["packages"]
    if package["manifest_path"] == sys.argv[1]
)
target = next(
    (
        target for target in package["targets"]
        if "bin" in target["kind"] or "cdylib" in target["crate_types"]
    ),
    None,
)
if target is None:
    raise SystemExit("manifest has no Wasm binary or cdylib target")
if "bin" in target["kind"]:
    print(target["name"] + ".wasm")
else:
    print(target["name"].replace("-", "_") + ".wasm")
' "$manifest_abs")
  wasm="$target_dir/wasm32-wasip2/release/$artifact_name"
fi
[[ -f "$wasm" ]] || { echo "Missing Wasm artifact: $wasm" >&2; exit 1; }

deploy_args=(deploy-app \
  --state-file "$state_file" \
  --app "$app" \
  --version "$version" \
  --namespace "$namespace" \
  --wasm "$wasm" \
  --route-host "$route_host" \
  --fuel "$fuel" \
  --memory-mb "$memory_mb" \
  --health-check-path "$health_path")
deploy_args+=(--max-outbound-connections "$max_outbound_connections")
deploy_args+=(--max-open-fds "$max_open_fds")
if [[ -n "$rate_limit_rps" ]]; then
  deploy_args+=(--rate-limit-rps "$rate_limit_rps")
  [[ -z "$rate_limit_burst" ]] || deploy_args+=(--rate-limit-burst "$rate_limit_burst")
  [[ -z "$rate_limit_per_ip" ]] || deploy_args+=(--rate-limit-per-ip "$rate_limit_per_ip")
elif [[ -n "$rate_limit_burst" || -n "$rate_limit_per_ip" ]]; then
  echo "--rate-limit-burst and --rate-limit-per-ip require --rate-limit-rps." >&2
  exit 2
fi
if [[ -n "$target_node" ]]; then
  deploy_args+=(--target-node "$target_node")
fi
for cidr in "${allowed_cidrs[@]}"; do
  deploy_args+=(--allowed-cidr "$cidr")
done
for cidr in "${denied_cidrs[@]}"; do
  deploy_args+=(--denied-cidr "$cidr")
done
for path in "${allowed_filesystem_paths[@]}"; do
  deploy_args+=(--allowed-filesystem-path "$path")
done
for route_path in "${route_paths[@]}"; do
  deploy_args+=(--route-path "$route_path")
done
for env_var in "${env_vars[@]}"; do
  deploy_args+=(--env "$env_var")
done
WASM_CTL_AUTH_TOKEN="$auth_token" "$cli" "${deploy_args[@]}"

proxy_addr=$(python3 - "$state_file" "$target_node" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    state = json.load(stream)
nodes = state.get("nodes", [])
if not nodes:
    raise SystemExit("state contains no platform nodes")
target = sys.argv[2]
node = next((item for item in nodes if item.get("id") == target), None) if target else nodes[0]
if node is None:
    raise SystemExit(f"target node not found in state: {target}")
print(node["proxy_addr"])
PY
)

verification_target="node proxy"
services_file="${state_file}.services.json"
if [[ "$verify_direct_node" != true && -f "$services_file" ]]; then
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
    --max-time 5 --header "Host: $route_host" "http://$proxy_addr$verify_path" || true)
  body_matches=true
  if [[ -n "$verify_contains" ]] && ! grep -Fq -- "$verify_contains" "$response_file"; then
    body_matches=false
  fi
  if [[ "$status" =~ ^2[0-9][0-9]$ && "$body_matches" == true ]]; then
    echo "Verified $namespace:$app:$version through $verification_target at http://$proxy_addr$verify_path (Host: $route_host)"
    cat "$response_file"
    printf '\n'
    exit 0
  fi
  sleep 2
done

echo "Deployment verification timed out after ${timeout}s (last HTTP status: ${status:-none}, expected body text: ${verify_contains:-<not configured>})." >&2
exit 1

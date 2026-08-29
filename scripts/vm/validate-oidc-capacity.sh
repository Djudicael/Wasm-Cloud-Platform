#!/usr/bin/env bash
set -euo pipefail

state_file=.prod-validation-single-host-state.json
report_dir=/tmp/wasm-cloud-platform-oidc-capacity
soak_seconds=120
only_soak=false
expected_targets=10
auth_token=${WASM_CTL_AUTH_TOKEN:-local-test-write-token-change-me}

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state file}; shift 2 ;;
    --report-dir) report_dir=${2:?missing report directory}; shift 2 ;;
    --soak-seconds) soak_seconds=${2:?missing soak duration}; shift 2 ;;
    --only-soak) only_soak=true; shift ;;
    --expected-targets) expected_targets=${2:?missing target count}; shift 2 ;;
    -h|--help)
      echo "Usage: validate-oidc-capacity.sh [--state-file FILE] [--report-dir DIR] [--soak-seconds N] [--only-soak] [--expected-targets N]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ -f "$state_file" ]] || { echo "Missing state file: $state_file" >&2; exit 1; }
[[ "$soak_seconds" =~ ^[1-9][0-9]*$ ]] || { echo "--soak-seconds must be positive" >&2; exit 2; }

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
cd "$repo_root"
target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
export CARGO_TARGET_DIR=$target_dir
cargo build -p vm-testbed --bin http-benchmark --bin vm-testbed-cli
cargo build -p ctl --bin wasm-ctl
benchmark="$target_dir/debug/http-benchmark"
testbed="$target_dir/debug/vm-testbed-cli"
ctl="$target_dir/debug/wasm-ctl"
run_privileged() {
  if ((EUID == 0)); then
    "$@"
  else
    sudo -n "$@" || {
      echo "Privileged testbed operation failed. Run sudo -v immediately before this script." >&2
      return 1
    }
  fi
}

front_door=$(python3 - "$state_file.services.json" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    print(json.load(stream)["front_door"]["bind"])
PY
)
nats_url=$(python3 - "$state_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    print(json.load(stream)["nats_url"])
PY
)
node_id=$(python3 - "$state_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    print(json.load(stream)["nodes"][0]["id"])
PY
)
public_url="http://$front_door"

[[ ! -e "$report_dir" ]] || { echo "Report path already exists: $report_dir" >&2; exit 1; }
mkdir -m 700 -p "$report_dir"
login_payload="$report_dir/login.json"
python3 - > "$login_payload" <<'PY'
import json
print(json.dumps({"email":"admin@example.com","password":"Admin123","client_id":"admin-ui"}))
PY
chmod 600 "$login_payload"

run_one() {
  local stage=$1 route=$2 method=$3 rate=$4 seconds=$5 path=$6
  local count concurrency output
  count=$(python3 - "$rate" "$seconds" <<'PY'
import math, sys
print(max(1, math.ceil(float(sys.argv[1]) * int(sys.argv[2]))))
PY
)
  concurrency=$(python3 - "$rate" <<'PY'
import math, sys
print(max(2, min(32, math.ceil(float(sys.argv[1]) * 2))))
PY
)
  output="$report_dir/${stage}-${route}.json"
  args=(--url "$public_url$path" --host localhost --method "$method"
    --requests "$count" --concurrency "$concurrency" --warmup-requests 1
    --rate-per-second "$rate" --expected-status 200,429)
  if [[ "$method" == POST ]]; then
    args+=(--content-type application/json --body-file "$login_payload")
  fi
  "$benchmark" "${args[@]}" > "$output"
}

run_mix() {
  local stage=$1 total_rate=$2 seconds=$3
  echo "=== $stage: ${total_rate} req/s for ${seconds}s ==="
  local frontend discovery ready login
  read -r frontend discovery ready login < <(python3 - "$total_rate" <<'PY'
import sys
r=float(sys.argv[1])
print(r*.50, r*.25, r*.20, r*.05)
PY
)
  run_one "$stage" frontend GET "$frontend" "$seconds" / & p1=$!
  run_one "$stage" discovery GET "$discovery" "$seconds" /.well-known/openid-configuration & p2=$!
  run_one "$stage" readiness GET "$ready" "$seconds" /health/ready & p3=$!
  run_one "$stage" login POST "$login" "$seconds" /oidc/login & p4=$!
  wait "$p1"; wait "$p2"; wait "$p3"; wait "$p4"
}

curl -fsS "$public_url/health/ready" | grep -q '"database":"ok"'
curl -fsS http://127.0.0.1:9095/-/ready >/dev/null

if [[ "$only_soak" == true ]]; then
  run_mix soak-5 5 "$soak_seconds"
else
  run_mix baseline 5 20
  run_mix ramp-15 15 20
  run_mix ramp-30 30 20
  run_mix spike-60 60 20

  echo "=== one node unavailable: conservative 5 req/s for 30s ==="
  node_stopped=false
  recover_node() {
    if [[ "$node_stopped" == true ]]; then
      run_privileged "$testbed" restart-node --state-file "$state_file" --id "$node_id" || true
    fi
  }
  trap recover_node EXIT
  WASM_CTL_AUTH_TOKEN="$auth_token" "$ctl" --nats-url "$nats_url" node --target "$node_id" drain --timeout-secs 30
  run_privileged "$testbed" kill --state-file "$state_file" --id "$node_id"
  node_stopped=true
  run_mix one-node-unavailable 5 30
  run_privileged "$testbed" restart-node --state-file "$state_file" --id "$node_id"
  node_stopped=false
fi

deadline=$((SECONDS + 120))
until curl -fsS "$public_url/health/ready" | grep -q '"database":"ok"'; do
  ((SECONDS < deadline)) || { echo "OIDC did not recover after node restart" >&2; exit 1; }
  sleep 2
done

python3 - "$report_dir" > "$report_dir/summary.json" <<'PY'
import glob, json, os, sys
root=sys.argv[1]
stages={}
for path in sorted(glob.glob(os.path.join(root, "*.json"))):
    if os.path.basename(path) in {"login.json", "summary.json"}:
        continue
    with open(path, encoding="utf-8") as stream:
        row=json.load(stream)
    name=os.path.basename(path)[:-5]
    stage, route=name.rsplit("-", 1)
    stages.setdefault(stage, {})[route]=row
print(json.dumps({"workload_mix":{"frontend":.50,"discovery":.25,"readiness":.20,"login":.05},
                  "stages":stages}, indent=2, sort_keys=True))
PY
chmod 600 "$report_dir/summary.json"

curl -fsS "$public_url/health/ready" | grep -q '"database":"ok"'
python3 - "$expected_targets" <<'PY'
import json, urllib.request
import sys
with urllib.request.urlopen("http://127.0.0.1:9095/api/v1/query?query=sum(up)") as response:
    value=float(json.load(response)["data"]["result"][0]["value"][1])
expected=float(sys.argv[1])
assert value == expected, (value, expected)
with urllib.request.urlopen("http://127.0.0.1:9095/api/v1/alerts") as response:
    alerts=json.load(response)["data"]["alerts"]
firing=[a for a in alerts if a["state"] == "firing"]
if expected < 10:
    firing=[a for a in firing if a["labels"].get("alertname") != "PlatformNodeDown"]
assert not firing, firing
PY

echo "OIDC capacity validation passed. Evidence: $report_dir/summary.json"

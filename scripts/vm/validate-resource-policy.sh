#!/usr/bin/env bash
set -euo pipefail

state_file=.vm-testbed-state.json
capacity_summary=
oversized_wasm=
output_file=
auth_token=${WASM_CTL_AUTH_TOKEN:-local-test-write-token-change-me}

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --capacity-summary) capacity_summary=${2:?missing capacity summary}; shift 2 ;;
    --oversized-wasm) oversized_wasm=${2:?missing Wasm component}; shift 2 ;;
    --output) output_file=${2:?missing output path}; shift 2 ;;
    -h|--help)
      echo "Usage: validate-resource-policy.sh --state-file FILE --capacity-summary FILE --oversized-wasm FILE --output FILE"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"
[[ -f "$state_file" && -f "$capacity_summary" && -f "$oversized_wasm" ]] || {
  echo "State, capacity summary, and oversized-test Wasm component are required." >&2
  exit 1
}
[[ -n "$output_file" ]] || { echo "--output is required." >&2; exit 2; }

target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
cli="$target_dir/debug/vm-testbed-cli"
[[ -x "$cli" ]] || CARGO_TARGET_DIR="$target_dir" cargo build -p vm-testbed --bin vm-testbed-cli

mapfile -t topology < <(python3 - "$state_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    state = json.load(stream)
print(state["node_memory_mb"])
for node in state["nodes"]:
    print(f'{node["id"]}\t{node["admin_addr"]}')
PY
)
node_memory_mb=${topology[0]}
node_records=("${topology[@]:1}")
(( ${#node_records[@]} >= 3 )) || { echo "Production-like validation requires at least three nodes." >&2; exit 1; }

temporary_dir=$(mktemp -d)
chmod 700 "$temporary_dir"
trap 'rm -rf -- "$temporary_dir"' EXIT
observed="$temporary_dir/nodes.tsv"
: > "$observed"

for record in "${node_records[@]}"; do
  IFS=$'\t' read -r node_id admin_addr <<< "$record"
  health=$(curl -fsS --max-time 10 -H "Authorization: Bearer $auth_token" "http://$admin_addr/healthz")
  jq -e '.status == "healthy" and .accepting_requests == true' <<< "$health" >/dev/null
  metrics=$(curl -fsS --max-time 10 -H "Authorization: Bearer $auth_token" "http://$admin_addr/metrics")
  metric() { awk -v name="$1" '$1 == name {print $2; found=1} END {if (!found) exit 1}' <<< "$metrics"; }
  disk_free_mb=$(metric wasm_node_disk_free_mb)
  disk_min_free_mb=$(metric wasm_node_disk_min_free_mb)
  disk_free_inodes=$(metric wasm_node_disk_free_inodes)
  disk_min_free_inodes=$(metric wasm_node_disk_min_free_inodes)
  memory_used_mb=$(metric wasm_node_memory_used_mb)
  memory_limit_mb=$(metric wasm_node_memory_limit_mb)
  memory_usage_percent=$(metric wasm_node_memory_usage_percent)
  (( disk_min_free_mb > 0 && disk_free_mb >= 2 * disk_min_free_mb ))
  (( disk_min_free_inodes > 0 && disk_free_inodes >= 2 * disk_min_free_inodes ))
  (( memory_limit_mb > 0 && memory_limit_mb < node_memory_mb ))
  (( memory_usage_percent < 85 ))
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$node_id" "$disk_free_mb" "$disk_min_free_mb" "$disk_free_inodes" \
    "$disk_min_free_inodes" "$memory_used_mb" "$memory_limit_mb" "$memory_usage_percent" >> "$observed"
done

python3 - "$capacity_summary" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    summary = json.load(stream)
rows = [row for stage in summary["stages"].values() for row in stage.values()]
assert sum(row["requests"] for row in rows) == 600
assert sum(row["failed"] for row in rows) == 0
assert all(row["status_counts"] == {"200": row["requests"]} for row in rows)
PY

# The testbed CLI deliberately publishes the same event shape as the real
# control plane. Every node must reject this 2-GiB declared pool against the
# image's 1.5-GiB node budget before persisting application state.
oversized_app=validation/p10-08-oversized:v1
WASM_CTL_AUTH_TOKEN="$auth_token" "$cli" deploy-app \
  --state-file "$state_file" --app p10-08-oversized --version v1 \
  --namespace validation --wasm "$oversized_wasm" --memory-mb 512 \
  --max-instances 4 --health-check-path none >/dev/null
sleep 8
for record in "${node_records[@]}"; do
  IFS=$'\t' read -r _ admin_addr <<< "$record"
  apps=$(curl -fsS --max-time 10 -H "Authorization: Bearer $auth_token" "http://$admin_addr/admin/apps")
  jq -e --arg id "$oversized_app" '[.[] | select(.id == $id)] | length == 0' <<< "$apps" >/dev/null
done

curl -fsS --max-time 10 http://127.0.0.1:8088/health/ready | \
  jq -e '.status == "ready" and .checks.database == "ok"' >/dev/null

python3 - "$observed" "$node_memory_mb" "$capacity_summary" "$output_file" <<'PY'
import json, os, sys, tempfile
observed, node_memory_mb, capacity_summary, output = sys.argv[1:]
nodes = []
with open(observed, encoding="utf-8") as stream:
    for line in stream:
        values = line.rstrip().split("\t")
        nodes.append({
            "node_id": values[0],
            "disk_free_mb": int(values[1]),
            "disk_min_free_mb": int(values[2]),
            "disk_free_inodes": int(values[3]),
            "disk_min_free_inodes": int(values[4]),
            "memory_used_mb": int(values[5]),
            "memory_limit_mb": int(values[6]),
            "memory_usage_percent": int(values[7]),
        })
result = {
    "result": "pass",
    "node_memory_mb": int(node_memory_mb),
    "nodes": nodes,
    "capacity_summary": os.path.abspath(capacity_summary),
    "capacity_requests": 600,
    "capacity_failures": 0,
    "oversized_application": {
        "declared_pool_mb": 2048,
        "persisted_nodes": 0,
    },
    "oidc_database_readiness": "ok",
}
directory = os.path.dirname(os.path.abspath(output))
os.makedirs(directory, exist_ok=True)
fd, temporary = tempfile.mkstemp(prefix=".resource-policy-", dir=directory, text=True)
try:
    with os.fdopen(fd, "w", encoding="utf-8") as stream:
        json.dump(result, stream, indent=2)
        stream.write("\n")
    os.replace(temporary, output)
except BaseException:
    try: os.unlink(temporary)
    except FileNotFoundError: pass
    raise
print(json.dumps(result, indent=2))
PY

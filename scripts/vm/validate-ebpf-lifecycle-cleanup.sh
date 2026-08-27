#!/usr/bin/env bash
set -euo pipefail

state_file=.prod-validation-single-host-state.json
cycles=3
auth_token=${WASM_CTL_AUTH_TOKEN:-local-test-write-token-change-me}
app_name=ebpf-lifecycle
app_version=v1
namespace=default
route_host=ebpf-lifecycle.internal

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --cycles) cycles=${2:?missing cycle count}; shift 2 ;;
    --auth-token) auth_token=${2:?missing auth token}; shift 2 ;;
    -h|--help)
      echo "Usage: validate-ebpf-lifecycle-cleanup.sh [--state-file PATH] [--cycles N] [--auth-token TOKEN]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
[[ "$cycles" =~ ^[1-9][0-9]*$ ]] || { echo "--cycles must be a positive integer." >&2; exit 2; }
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
cd "$repo_root"
[[ -f "$state_file" ]] || { echo "Missing topology state: $state_file" >&2; exit 1; }
for command_name in cargo curl jq; do
  command -v "$command_name" >/dev/null || { echo "Missing required command: $command_name" >&2; exit 1; }
done

target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
CARGO_TARGET_DIR="$target_dir" cargo build -p vm-testbed --bin vm-testbed-cli
cli="$target_dir/debug/vm-testbed-cli"
app_id="${namespace}/${app_name}:${app_version}"
mapfile -t node_ids < <(jq -er '.nodes[].id' "$state_file")
mapfile -t node_ips < <(jq -er '.nodes[].ip' "$state_file")
mapfile -t proxy_addrs < <(jq -er '.nodes[].proxy_addr' "$state_file")
((${#node_ids[@]} >= 3)) || { echo "Lifecycle validation requires at least three platform nodes." >&2; exit 1; }

if [[ -n ${WSL_DISTRO_NAME:-} ]] && command -v wsl.exe >/dev/null; then
  run_privileged() { wsl.exe -u root -- "$@"; }
else
  sudo -v
  run_privileged() { sudo -E "$@"; }
fi

admin_get() {
  local ip=$1 path=$2
  curl -fsS --max-time 5 -H "Authorization: Bearer $auth_token" "http://${ip}:9090${path}"
}

snapshot() { admin_get "$1" /admin/ebpf/identities; }

assert_node_monitor_base() {
  local ip=$1
  admin_get "$ip" /admin/ebpf/status | jq -e \
    '.ebpf_active == true and .attached_programs == 7 and .monitoring_degraded == false' >/dev/null
  snapshot "$ip" | jq -e \
    '.bpffs_mounted == true and .pinned_entries == 0 and
     (.kernel_map_entry_counts | length) == 5' >/dev/null
}

assert_snapshot_consistent() {
  local ip=$1 body active_count
  body=$(snapshot "$ip")
  active_count=$(jq '.active_tids | length' <<<"$body")
  jq -e --argjson active_count "$active_count" \
    '.bpffs_mounted == true and .pinned_entries == 0 and
     (.kernel_map_entry_counts | length) == 5 and
     ([.kernel_map_entry_counts[]] | all(. == $active_count))' <<<"$body" >/dev/null
}

wait_for_identity_state() {
  local ip=$1 expected=$2 deadline=$((SECONDS + 45)) body present
  while ((SECONDS < deadline)); do
    body=$(snapshot "$ip" 2>/dev/null || printf '{}')
    present=$(jq -r --arg app "$app_id" '[.active_tids[]? | select(.app_id == $app)] | length' <<<"$body")
    if [[ "$present" == "$expected" ]]; then
      assert_snapshot_consistent "$ip"
      return 0
    fi
    sleep 1
  done
  echo "Identity state for $app_id on $ip did not become $expected." >&2
  return 1
}

identity_tuple() {
  snapshot "$1" | jq -er --arg app "$app_id" \
    '.active_tids[] | select(.app_id == $app) | "\(.tid):\(.registered_at_ns)"'
}

assert_fresh_identity() {
  local previous=$1 current=$2 context=$3
  [[ "$current" != "$previous" ]] || {
    echo "Identity generation was reused during $context: $current" >&2
    exit 1
  }
}

request_direct() {
  local proxy=$1
  curl -fsS --max-time 10 -H "Host: $route_host" "http://${proxy}/health" >/dev/null
}

wait_for_direct() {
  local proxy=$1 deadline=$((SECONDS + 45))
  until request_direct "$proxy" 2>/dev/null; do
    ((SECONDS < deadline)) || {
      echo "Application did not become reachable through $proxy." >&2
      return 1
    }
    sleep 1
  done
}

wait_for_front_door() {
  local deadline=$((SECONDS + 45))
  until curl -fsS --max-time 10 -H "Host: $route_host" \
    http://127.0.0.1:8088/health >/dev/null 2>&1; do
    ((SECONDS < deadline)) || {
      echo "Application did not reconverge through the HAProxy front door." >&2
      return 1
    }
    sleep 1
  done
}

warm_every_node() {
  local index
  for index in "${!node_ips[@]}"; do
    wait_for_direct "${proxy_addrs[$index]}"
    wait_for_identity_state "${node_ips[$index]}" 1
  done
}

wait_for_removed_everywhere() {
  local index
  for index in "${!node_ips[@]}"; do
    wait_for_identity_state "${node_ips[$index]}" 0
  done
}

assert_route_removed() {
  local proxy status deadline
  for proxy in "${proxy_addrs[@]}"; do
    deadline=$((SECONDS + 45))
    while ((SECONDS < deadline)); do
      status=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 \
        -H "Host: $route_host" "http://${proxy}/health" || true)
      [[ ! "$status" =~ ^2[0-9][0-9]$ ]] && break
      sleep 1
    done
    [[ ! "$status" =~ ^2[0-9][0-9]$ ]] || {
      echo "Removed route still served successfully through $proxy." >&2
      return 1
    }
  done
}

assert_metric_series_removed() {
  local ip deadline body
  for ip in "${node_ips[@]}"; do
    deadline=$((SECONDS + 45))
    while ((SECONDS < deadline)); do
      body=$(admin_get "$ip" /metrics)
      if ! grep -Fq "wasm_node_app_healthy_instances{app=\"$app_id\"}" <<<"$body" &&
         ! grep -Fq "wasm_node_app_total_instances{app=\"$app_id\"}" <<<"$body"; then
        break
      fi
      sleep 1
    done
    if grep -Fq "wasm_node_app_healthy_instances{app=\"$app_id\"}" <<<"$body" ||
       grep -Fq "wasm_node_app_total_instances{app=\"$app_id\"}" <<<"$body"; then
      echo "Stale application metric series remains on $ip." >&2
      return 1
    fi
  done
}

assert_no_new_stale_events() {
  local index log first_line
  for index in "${!node_ids[@]}"; do
    log="/tmp/vm-testbed-${node_ids[$index]}/serial.log"
    first_line=$(( $(wc -l <"$log") + 1 ))
    for _ in {1..10}; do
      curl -sS -o /dev/null --max-time 2 -H "Host: $route_host" \
        "http://${proxy_addrs[$index]}/health" || true
    done
    sleep 1
    if tail -n +"$first_line" "$log" | grep -F '"target":"ebpf_monitor::actions"' | grep -Fq "\"app_id\":\"$app_id\""; then
      echo "A removed identity received a new eBPF event on ${node_ids[$index]}." >&2
      return 1
    fi
  done
}

undeploy() {
  "$cli" undeploy-app --state-file "$state_file" --app-id "$app_id"
  wait_for_removed_everywhere
  assert_route_removed
  assert_metric_series_removed
}

deploy() {
  CARGO_TARGET_DIR="$target_dir" bash scripts/vm/deploy-test-application.sh \
    --state-file "$state_file" \
    --app "$app_name" \
    --version "$app_version" \
    --namespace "$namespace" \
    --manifest apps/http-hello-component/Cargo.toml \
    --route-host "$route_host" \
    --target-node "${node_ids[0]}" \
    --fuel 5000000000 \
    --timeout 90
  warm_every_node
}

for ip in "${node_ips[@]}"; do
  assert_node_monitor_base "$ip"
done

# Re-issue removal for the previous Part 5 fixture after the route-cleanup fix
# is live. This clears its pre-fix persisted route on every node.
"$cli" undeploy-app --state-file "$state_file" --app-id default/ebpf-failure:v1
"$cli" undeploy-app --state-file "$state_file" --app-id "$app_id"
wait_for_removed_everywhere
assert_route_removed
assert_metric_series_removed

last_generation=
for cycle in $(seq 1 "$cycles"); do
  echo "=== lifecycle cycle $cycle/$cycles: deploy and attribute ==="
  deploy
  current_generation=$(identity_tuple "${node_ips[0]}")
  if [[ -n "$last_generation" ]]; then
    assert_fresh_identity "$last_generation" "$current_generation" "cycle $cycle redeploy"
  fi

  echo "=== lifecycle cycle $cycle/$cycles: stop and cold restart ==="
  before_stop=$current_generation
  response=$(curl -fsS --max-time 5 -X POST \
    -H "Authorization: Bearer $auth_token" \
    -H 'Content-Type: application/json' \
    --data '{"kill_largest":true,"kill_largest_reason":"phase6-part6-lifecycle"}' \
    "http://${node_ips[0]}:9090/admin/ebpf/config")
  jq -e '.actions | index("kill_largest") != null' <<<"$response" >/dev/null
  wait_for_identity_state "${node_ips[0]}" 0
  wait_for_direct "${proxy_addrs[0]}"
  wait_for_identity_state "${node_ips[0]}" 1
  after_stop=$(identity_tuple "${node_ips[0]}")
  assert_fresh_identity "$before_stop" "$after_stop" "cycle $cycle cold restart"

  if [[ "$cycle" == 1 ]]; then
    echo "=== lifecycle cycle 1: rolling node restart ==="
    for index in "${!node_ids[@]}"; do
      before_restart=$(identity_tuple "${node_ips[$index]}")
      run_privileged "$cli" restart-node --state-file "$state_file" --id "${node_ids[$index]}"
      wait_for_direct "${proxy_addrs[$index]}"
      wait_for_identity_state "${node_ips[$index]}" 1
      after_restart=$(identity_tuple "${node_ips[$index]}")
      assert_fresh_identity "$before_restart" "$after_restart" "rolling restart of ${node_ids[$index]}"
      wait_for_front_door
      assert_node_monitor_base "${node_ips[$index]}"
    done
  fi

  last_generation=$(identity_tuple "${node_ips[0]}")
  echo "=== lifecycle cycle $cycle/$cycles: remove and prove cleanup ==="
  undeploy
  sleep 2
  assert_no_new_stale_events
  for ip in "${node_ips[@]}"; do
    assert_node_monitor_base "$ip"
  done
done

# Tombstones exist only to label already-queued exit events. They must expire,
# while active and kernel maps stay empty and no pins accumulate.
sleep 32
for ip in "${node_ips[@]}"; do
  snapshot "$ip" | jq -e \
    '.active_tids == [] and .recent_tombstones == 0 and .port_bindings == 0 and
     ([.kernel_map_entry_counts[]] | all(. == 0)) and
     .bpffs_mounted == true and .pinned_entries == 0' >/dev/null
done

# Prometheus must no longer evaluate application readiness for the removed app,
# and the HAProxy scrape repaired before this phase must remain healthy.
prometheus_alerts=$(curl -fsS --max-time 5 http://127.0.0.1:9095/api/v1/alerts)
jq -e --arg app "$app_id" \
  'all(.data.alerts[]?; .labels.app != $app)' <<<"$prometheus_alerts" >/dev/null
prometheus_targets=$(curl -fsS --max-time 5 http://127.0.0.1:9095/api/v1/targets)
jq -e \
  'any(.data.activeTargets[]; .labels.job == "haproxy" and .health == "up")' \
  <<<"$prometheus_targets" >/dev/null

echo "eBPF lifecycle cleanup passed: $cycles deploy/stop/restart/remove cycles for $app_id."

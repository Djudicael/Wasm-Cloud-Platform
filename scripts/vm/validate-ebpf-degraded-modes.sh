#!/usr/bin/env bash
set -euo pipefail

state_file=.prod-validation-single-host-state.json
node_id=
canary_url=http://127.0.0.1:8088/health
canary_host=ebpf-failure.internal
alertmanager_url=http://127.0.0.1:9093
auth_token=${WASM_CTL_AUTH_TOKEN:-local-test-write-token-change-me}

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --node-id) node_id=${2:?missing node id}; shift 2 ;;
    --canary-url) canary_url=${2:?missing canary URL}; shift 2 ;;
    --canary-host) canary_host=${2:?missing canary host}; shift 2 ;;
    --alertmanager-url) alertmanager_url=${2:?missing Alertmanager URL}; shift 2 ;;
    --auth-token) auth_token=${2:?missing auth token}; shift 2 ;;
    -h|--help)
      echo "Usage: validate-ebpf-degraded-modes.sh [--state-file PATH] [--node-id ID] [--canary-url URL] [--canary-host HOST] [--alertmanager-url URL] [--auth-token TOKEN]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
cd "$repo_root"
[[ -f "$state_file" ]] || { echo "Missing topology state: $state_file" >&2; exit 1; }
[[ -f "${state_file}.services.json" ]] || {
  echo "Missing companion service state; provision observability before this validation." >&2
  exit 1
}
for command_name in curl jq objcopy readelf; do
  command -v "$command_name" >/dev/null || { echo "Missing required command: $command_name" >&2; exit 1; }
done

target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
CARGO_TARGET_DIR="$target_dir" cargo build -p vm-testbed --bin vm-testbed-cli
cli="$target_dir/debug/vm-testbed-cli"

if [[ -n ${WSL_DISTRO_NAME:-} ]] && command -v wsl.exe >/dev/null; then
  run_privileged() { wsl.exe -u root -- "$@"; }
else
  sudo -v
  run_privileged() { sudo -E "$@"; }
fi

if [[ -z "$node_id" ]]; then
  node_id=$(jq -er '.nodes[0].id' "$state_file")
fi
node_ip=$(jq -er --arg id "$node_id" '.nodes[] | select(.id == $id) | .ip' "$state_file")

assert_canary() {
  local deadline=$((SECONDS + 30))
  local status
  while ((SECONDS < deadline)); do
    status=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 -H "Host: $canary_host" "$canary_url" || true)
    [[ "$status" == 200 ]] && return 0
    sleep 1
  done
  echo "Canary did not recover from HTTP $status within 30s during eBPF fault validation." >&2
  return 1
}

assert_other_nodes_healthy() {
  while IFS= read -r ip; do
    [[ "$ip" == "$node_ip" ]] && continue
    curl -fsS --max-time 5 "http://${ip}:9090/readyz" >/dev/null
  done < <(jq -r '.nodes[].ip' "$state_file")
}

wait_for_alert() {
  local alert_name=$1
  local deadline=$((SECONDS + 90))
  local alerts='[]'
  while ((SECONDS < deadline)); do
    alerts=$(curl -sS --max-time 5 "${alertmanager_url}/api/v2/alerts" 2>/dev/null || printf '[]')
    if jq -e --arg alert_name "$alert_name" \
      'any(.[]; .labels.alertname == $alert_name)' <<< "$alerts" >/dev/null 2>&1; then
      printf '%s' "$alerts"
      return 0
    fi
    sleep 2
  done
  echo "Alert $alert_name did not fire within 90s." >&2
  return 1
}

assert_monitor_state() {
  local active=$1
  local degraded=$2
  local reason=$3
  local status
  status=$(curl -fsS --max-time 5 -H "Authorization: Bearer $auth_token" "http://${node_ip}:9090/admin/ebpf/status")
  jq -e \
    --argjson active "$active" \
    --argjson degraded "$degraded" \
    --arg reason "$reason" \
    '.ebpf_active == $active and .monitoring_degraded == $degraded and ((($reason == "none") and (.monitoring_degraded_reason == null)) or (.monitoring_degraded_reason == $reason))' \
    <<< "$status" >/dev/null
  curl -fsS --max-time 5 -H "Authorization: Bearer $auth_token" "http://${node_ip}:9090/metrics" | grep -qx "wasm_ebpf_active $([[ "$active" == true ]] && echo 1 || echo 0)"
  curl -fsS --max-time 5 -H "Authorization: Bearer $auth_token" "http://${node_ip}:9090/metrics" | grep -qx "wasm_ebpf_monitoring_degraded $([[ "$degraded" == true ]] && echo 1 || echo 0)"
}

assert_optional_readiness() {
  local report
  report=$(curl -fsS --max-time 5 "http://${node_ip}:9090/readyz")
  jq -e '.status == "degraded" and any(.dependencies[]; .name == "ebpf_monitoring" and .status == "degraded")' <<< "$report" >/dev/null
}

restart_clean() {
  run_privileged "$cli" restart-node --state-file "$state_file" --id "$node_id"
  assert_monitor_state true false none
  assert_canary
  assert_other_nodes_healthy
}

run_optional_fault() {
  local fault=$1
  local active=$2
  local reason=$3
  echo "=== optional eBPF fault: $fault ==="
  run_privileged "$cli" restart-node \
    --state-file "$state_file" \
    --id "$node_id" \
    --ebpf-test-fault "$fault"
  [[ "$fault" == consumer-exit ]] && sleep 5
  assert_monitor_state "$active" true "$reason"
  assert_optional_readiness
  assert_canary
  assert_other_nodes_healthy
  restart_clean
}

assert_monitor_state true false none
assert_canary
assert_other_nodes_healthy

echo "=== actual guest failure: missing-capability ==="
run_privileged "$cli" restart-node \
  --state-file "$state_file" \
  --id "$node_id" \
  --drop-ebpf-capabilities
assert_monitor_state false true missing_capability
assert_optional_readiness
assert_canary
assert_other_nodes_healthy
restart_clean

run_optional_fault permission-denied false insufficient_privileges
run_optional_fault program-rejected false program_load_rejected
run_optional_fault probe-unavailable true partial_probe_set

# Remove BTF from a disposable kernel copy. This exercises the real guest
# preflight rather than the deterministic loader hook used by the other cases.
no_btf_kernel=$(mktemp /tmp/wcp-vmlinux-no-btf.XXXXXX)
trap 'rm -f -- "$no_btf_kernel"' EXIT
objcopy --remove-section=.BTF --remove-section=.BTF_ids assets/vmlinux-6.1 "$no_btf_kernel"
if readelf -S "$no_btf_kernel" | grep -q '[.]BTF'; then
  echo "Disposable missing-BTF kernel still contains a BTF section." >&2
  exit 1
fi

# Hold the real missing-BTF state long enough to prove the monitoring-specific
# alert fires while the node and application remain available.
echo "=== actual guest failure with alert verification: missing-btf ==="
run_privileged "$cli" restart-node \
  --state-file "$state_file" \
  --id "$node_id" \
  --kernel "$no_btf_kernel"
assert_monitor_state false true missing_btf
assert_optional_readiness
assert_canary
assert_other_nodes_healthy
alerts=$(wait_for_alert EbpfMonitoringUnavailable)
jq -e 'all(.[]; .labels.alertname != "PlatformNodeDown")' <<< "$alerts" >/dev/null
restart_clean

run_optional_fault consumer-exit false consumer_exited

echo "=== mandatory eBPF failure: missing-btf ==="
run_privileged "$cli" restart-node \
  --state-file "$state_file" \
  --id "$node_id" \
  --kernel "$no_btf_kernel" \
  --ebpf-required \
  --expect-unhealthy
if curl -fsS --max-time 3 "http://${node_ip}:9090/readyz" >/dev/null 2>&1; then
  echo "Mandatory eBPF failure unexpectedly remained ready." >&2
  exit 1
fi
assert_canary
assert_other_nodes_healthy
restart_clean

echo "eBPF failure and degraded-mode validation passed for $node_id ($node_ip)."

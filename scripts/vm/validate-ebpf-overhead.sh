#!/usr/bin/env bash
set -euo pipefail

state_file=.prod-validation-single-host-state.json
node_id=
auth_token=${WASM_CTL_AUTH_TOKEN:-local-test-write-token-change-me}
requests=20000
concurrency=32
blocks=2
rounds_per_block=2
output=/tmp/phase6-part7-ebpf-overhead.json
app_name=ebpf-overhead
app_version=v1
namespace=default
route_host=ebpf-overhead.internal

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --node-id) node_id=${2:?missing node id}; shift 2 ;;
    --auth-token) auth_token=${2:?missing auth token}; shift 2 ;;
    --requests) requests=${2:?missing request count}; shift 2 ;;
    --concurrency) concurrency=${2:?missing concurrency}; shift 2 ;;
    --blocks) blocks=${2:?missing block count}; shift 2 ;;
    --rounds-per-block) rounds_per_block=${2:?missing round count}; shift 2 ;;
    --output) output=${2:?missing output path}; shift 2 ;;
    -h|--help)
      echo "Usage: validate-ebpf-overhead.sh [--state-file PATH] [--node-id ID] [--requests N] [--concurrency N] [--blocks N] [--rounds-per-block N] [--output PATH]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
for value in "$requests" "$concurrency" "$blocks" "$rounds_per_block"; do
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || { echo "Counts must be positive integers." >&2; exit 2; }
done
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(cd "$script_dir/../.." && pwd)
cd "$repo_root"
[[ -f "$state_file" ]] || { echo "Missing topology state: $state_file" >&2; exit 1; }
for command_name in cargo curl jq python3; do
  command -v "$command_name" >/dev/null || { echo "Missing required command: $command_name" >&2; exit 1; }
done

target_dir=${CARGO_TARGET_DIR:-/tmp/wasm-cloud-platform-target}
CARGO_TARGET_DIR="$target_dir" cargo build -p vm-testbed --bin vm-testbed-cli --bin http-benchmark
cli="$target_dir/debug/vm-testbed-cli"
benchmark="$target_dir/debug/http-benchmark"
if [[ -z "$node_id" ]]; then
  node_id=$(jq -er '.nodes[0].id' "$state_file")
fi
node_ip=$(jq -er --arg id "$node_id" '.nodes[] | select(.id == $id) | .ip' "$state_file")
proxy_addr=$(jq -er --arg id "$node_id" '.nodes[] | select(.id == $id) | .proxy_addr' "$state_file")
app_id="${namespace}/${app_name}:${app_version}"

if [[ -n ${WSL_DISTRO_NAME:-} ]] && command -v wsl.exe >/dev/null; then
  run_privileged() { wsl.exe -u root -- "$@"; }
else
  sudo -v
  run_privileged() { sudo -E "$@"; }
fi

admin_get() {
  curl -fsS --max-time 5 -H "Authorization: Bearer $auth_token" "http://${node_ip}:9090$1"
}

snapshot() { admin_get /admin/ebpf/identities; }

monitor_status() { admin_get /admin/ebpf/status; }

wait_for_application() {
  local deadline=$((SECONDS + 60)) body
  until curl -fsS --max-time 5 -H "Host: $route_host" "http://${proxy_addr}/" >/dev/null 2>&1; do
    ((SECONDS < deadline)) || { echo "Benchmark application did not become reachable." >&2; return 1; }
    sleep 1
  done
  until body=$(snapshot 2>/dev/null) && jq -e --arg app "$app_id" \
    'any(.active_tids[]?; .app_id == $app and .performance != null) and .process_performance != null and .clock_ticks_per_second > 0' \
    <<<"$body" >/dev/null; do
    ((SECONDS < deadline)) || { echo "Application performance identity did not become observable." >&2; return 1; }
    sleep 1
  done
}

wait_for_load_readiness() {
  local deadline=$((SECONDS + 45))
  until "$benchmark" --url "http://${proxy_addr}/" --host "$route_host" \
    --requests 200 --concurrency "$concurrency" --warmup-requests 0 >/dev/null 2>&1; do
    ((SECONDS < deadline)) || {
      echo "Application did not accept a concurrent mini-load without failures." >&2
      return 1
    }
    sleep 1
  done
}

deploy_canary() {
  CARGO_TARGET_DIR="$target_dir" bash scripts/vm/deploy-test-application.sh \
    --state-file "$state_file" --app "$app_name" --version "$app_version" --namespace "$namespace" \
    --manifest apps/http-hello-component/Cargo.toml --route-host "$route_host" \
    --target-node "$node_id" --fuel 5000000000 --timeout 90 --verify-direct-node \
    --rate-limit-rps 10000 --rate-limit-burst 20000 --rate-limit-per-ip 10000 >/dev/null
}

restart_baseline() {
  run_privileged "$cli" restart-node --state-file "$state_file" --id "$node_id" --drop-ebpf-capabilities
  monitor_status | jq -e \
    '.ebpf_active == false and .monitoring_degraded == true and .monitoring_degraded_reason == "missing_capability"' >/dev/null
  deploy_canary
  wait_for_application
  wait_for_load_readiness
}

restart_active() {
  run_privileged "$cli" restart-node --state-file "$state_file" --id "$node_id"
  monitor_status | jq -e \
    '.ebpf_active == true and .attached_programs == 7 and .monitoring_degraded == false' >/dev/null
  deploy_canary
  wait_for_application
  wait_for_load_readiness
}

restore_active=false
work_dir=$(mktemp -d /tmp/wcp-ebpf-overhead.XXXXXX)
cleanup() {
  result=$?
  if [[ "$restore_active" == true ]]; then
    restart_active >/dev/null 2>&1 || true
  fi
  rm -rf -- "$work_dir"
  exit "$result"
}
trap cleanup EXIT

metrics_snapshot() {
  admin_get /metrics | python3 -c '
import json, sys
names = {
    "events": "wasm_ebpf_events_processed_total",
    "parse_errors": "wasm_ebpf_events_parse_errors_total",
    "drops": "wasm_ebpf_ring_buffer_dropped_events_total",
    "saturations": "wasm_ebpf_dispatch_queue_saturations_total",
}
values = {key: 0.0 for key in names}
for line in sys.stdin:
    if not line or line.startswith("#"):
        continue
    metric, _, raw = line.rstrip().rpartition(" ")
    for key, name in names.items():
        if metric == name or metric.startswith(name + "{"):
            values[key] += float(raw)
print(json.dumps(values))
'
}

record_round() {
  local mode=$1 round=$2 destination=$3
  local before="$work_dir/${mode}-${round}-before.json"
  local after="$work_dir/${mode}-${round}-after.json"
  local metrics_before="$work_dir/${mode}-${round}-metrics-before.json"
  local metrics_after="$work_dir/${mode}-${round}-metrics-after.json"
  local samples="$work_dir/${mode}-${round}-samples.jsonl"
  local result="$work_dir/${mode}-${round}-result.json"

  snapshot >"$before"
  metrics_snapshot >"$metrics_before"
  : >"$samples"
  "$benchmark" --url "http://${proxy_addr}/" --host "$route_host" \
    --requests "$requests" --concurrency "$concurrency" --warmup-requests 0 >"$result" &
  local load_pid=$!
  while kill -0 "$load_pid" 2>/dev/null; do
    { snapshot && printf '\n'; } >>"$samples" || true
    sleep 0.2
  done
  wait "$load_pid"
  snapshot >"$after"
  metrics_snapshot >"$metrics_after"

  python3 - "$mode" "$round" "$app_id" "$before" "$after" "$metrics_before" \
    "$metrics_after" "$samples" "$result" >>"$destination" <<'PY'
import json, sys
mode, round_number, app_id = sys.argv[1:4]
paths = sys.argv[4:]
with open(paths[0], encoding="utf-8") as stream: before = json.load(stream)
with open(paths[1], encoding="utf-8") as stream: after = json.load(stream)
with open(paths[2], encoding="utf-8") as stream: metrics_before = json.load(stream)
with open(paths[3], encoding="utf-8") as stream: metrics_after = json.load(stream)
with open(paths[5], encoding="utf-8") as stream: benchmark = json.load(stream)
samples = [before, after]
with open(paths[4], encoding="utf-8") as stream:
    samples.extend(json.loads(line) for line in stream if line.strip())

def app(snapshot):
    return next(item["performance"] for item in snapshot["active_tids"] if item["app_id"] == app_id)
def delta(end, start, key):
    return max(0, end[key] - start[key])

node_before, node_after = before["process_performance"], after["process_performance"]
app_before, app_after = app(before), app(after)
ticks = before["clock_ticks_per_second"]
record = {
    "mode": mode,
    "round": int(round_number),
    "benchmark": benchmark,
    "node_cpu_seconds": (delta(node_after, node_before, "user_cpu_ticks") + delta(node_after, node_before, "system_cpu_ticks")) / ticks,
    "application_cpu_seconds": (delta(app_after, app_before, "user_cpu_ticks") + delta(app_after, app_before, "system_cpu_ticks")) / ticks,
    "node_context_switches": delta(node_after, node_before, "voluntary_context_switches") + delta(node_after, node_before, "nonvoluntary_context_switches"),
    "application_context_switches": delta(app_after, app_before, "voluntary_context_switches") + delta(app_after, app_before, "nonvoluntary_context_switches"),
    "rss_before_bytes": node_before["resident_memory_bytes"],
    "rss_after_bytes": node_after["resident_memory_bytes"],
    "rss_peak_bytes": max(sample["process_performance"]["resident_memory_bytes"] for sample in samples if sample.get("process_performance")),
    "ebpf_events": delta(metrics_after, metrics_before, "events"),
    "ebpf_parse_errors": delta(metrics_after, metrics_before, "parse_errors"),
    "ebpf_ring_drops": delta(metrics_after, metrics_before, "drops"),
    "ebpf_queue_saturations": delta(metrics_after, metrics_before, "saturations"),
}
print(json.dumps(record, sort_keys=True))
PY
}

# Acceptance thresholds are fixed before measurements begin.
# - throughput degradation <= 5%
# - median p99 increase <= max(5%, 1 ms)
# - node/application CPU seconds per 1k requests increase <= 15%
# - context switches per 1k requests increase <= 25%
# - additional peak RSS <= 64 MiB
# - zero request failures, parser errors, ring drops, and queue saturations
echo "Acceptance: throughput<=5% loss; p99<=max(5%,1ms); CPU<=15%; context switches<=25%; RSS<=64MiB; no failures/loss."

deploy_canary
wait_for_application
wait_for_load_readiness
restore_active=true

baseline_records="$work_dir/baseline.jsonl"
active_records="$work_dir/active.jsonl"
: >"$baseline_records"
: >"$active_records"
baseline_round=0
active_round=0
for block in $(seq 1 "$blocks"); do
  echo "=== block $block/$blocks: userspace fallback baseline ==="
  restart_baseline
  "$benchmark" --url "http://${proxy_addr}/" --host "$route_host" \
    --requests 2000 --concurrency "$concurrency" --warmup-requests 0 >/dev/null
  for _ in $(seq 1 "$rounds_per_block"); do
    baseline_round=$((baseline_round + 1)); echo "baseline round $baseline_round"
    record_round baseline "$baseline_round" "$baseline_records"
  done

  echo "=== block $block/$blocks: active eBPF ==="
  restart_active
  "$benchmark" --url "http://${proxy_addr}/" --host "$route_host" \
    --requests 2000 --concurrency "$concurrency" --warmup-requests 0 >/dev/null
  for _ in $(seq 1 "$rounds_per_block"); do
    active_round=$((active_round + 1)); echo "active round $active_round"
    record_round active "$active_round" "$active_records"
  done
done

mkdir -p "$(dirname "$output")"
python3 - "$baseline_records" "$active_records" "$output" <<'PY'
import json, math, statistics, sys

def read(path):
    with open(path, encoding="utf-8") as stream:
        return [json.loads(line) for line in stream if line.strip()]

baseline, active = read(sys.argv[1]), read(sys.argv[2])

def summarize(records):
    total_requests = sum(item["benchmark"]["requests"] for item in records)
    elapsed = sum(item["benchmark"]["elapsed_seconds"] for item in records)
    per_1000 = 1000 / total_requests
    return {
        "rounds": len(records),
        "requests": total_requests,
        "failed_requests": sum(item["benchmark"]["failed"] for item in records),
        "median_requests_per_second": statistics.median(item["benchmark"]["requests_per_second"] for item in records),
        "median_latency_ms": {
            percentile: statistics.median(item["benchmark"]["latency_ms"][percentile] for item in records)
            for percentile in ("p50", "p90", "p95", "p99", "max")
        },
        "node_cpu_seconds_per_1000_requests": sum(item["node_cpu_seconds"] for item in records) * per_1000,
        "application_cpu_seconds_per_1000_requests": sum(item["application_cpu_seconds"] for item in records) * per_1000,
        "node_context_switches_per_1000_requests": sum(item["node_context_switches"] for item in records) * per_1000,
        "application_context_switches_per_1000_requests": sum(item["application_context_switches"] for item in records) * per_1000,
        "minimum_rss_before_bytes": min(item["rss_before_bytes"] for item in records),
        "peak_rss_bytes": max(item["rss_peak_bytes"] for item in records),
        "ebpf_events": sum(item["ebpf_events"] for item in records),
        "ebpf_event_rate_per_second": sum(item["ebpf_events"] for item in records) / elapsed,
        "ebpf_parse_errors": sum(item["ebpf_parse_errors"] for item in records),
        "ebpf_ring_drops": sum(item["ebpf_ring_drops"] for item in records),
        "ebpf_queue_saturations": sum(item["ebpf_queue_saturations"] for item in records),
    }

base, enabled = summarize(baseline), summarize(active)
def percent_change(new, old):
    if old == 0:
        return 0.0 if new == 0 else math.inf
    return (new - old) / old * 100

throughput_loss = max(0.0, -percent_change(enabled["median_requests_per_second"], base["median_requests_per_second"]))
p99_increase = enabled["median_latency_ms"]["p99"] - base["median_latency_ms"]["p99"]
node_cpu_increase = percent_change(enabled["node_cpu_seconds_per_1000_requests"], base["node_cpu_seconds_per_1000_requests"])
app_cpu_increase = percent_change(enabled["application_cpu_seconds_per_1000_requests"], base["application_cpu_seconds_per_1000_requests"])
node_ctx_increase = percent_change(enabled["node_context_switches_per_1000_requests"], base["node_context_switches_per_1000_requests"])
app_ctx_increase = percent_change(enabled["application_context_switches_per_1000_requests"], base["application_context_switches_per_1000_requests"])
extra_peak_rss = max(0, enabled["peak_rss_bytes"] - base["peak_rss_bytes"])
p99_allowance = max(1.0, base["median_latency_ms"]["p99"] * 0.05)

checks = {
    "throughput_loss_within_5_percent": throughput_loss <= 5,
    "p99_increase_is_not_meaningful": p99_increase <= p99_allowance,
    "node_cpu_increase_within_15_percent": node_cpu_increase <= 15,
    "application_cpu_increase_within_15_percent": app_cpu_increase <= 15,
    "node_context_switch_increase_within_25_percent": node_ctx_increase <= 25,
    "application_context_switch_increase_within_25_percent": app_ctx_increase <= 25,
    "additional_peak_rss_within_64_mib": extra_peak_rss <= 64 * 1024 * 1024,
    "no_request_failures": base["failed_requests"] == enabled["failed_requests"] == 0,
    "no_parser_errors": enabled["ebpf_parse_errors"] == 0,
    "no_ring_drops": enabled["ebpf_ring_drops"] == 0,
    "no_queue_saturations": enabled["ebpf_queue_saturations"] == 0,
    "active_monitor_observed_events": enabled["ebpf_events"] > 0,
}
report = {
    "thresholds": {
        "throughput_loss_percent": 5,
        "p99_relative_increase_percent": 5,
        "p99_absolute_allowance_ms": 1,
        "cpu_increase_percent": 15,
        "context_switch_increase_percent": 25,
        "additional_peak_rss_mib": 64,
        "allowed_failures_or_event_loss": 0,
    },
    "baseline": base,
    "ebpf_enabled": enabled,
    "comparison": {
        "throughput_loss_percent": throughput_loss,
        "p99_increase_ms": p99_increase,
        "p99_allowance_ms": p99_allowance,
        "node_cpu_increase_percent": node_cpu_increase,
        "application_cpu_increase_percent": app_cpu_increase,
        "node_context_switch_increase_percent": node_ctx_increase,
        "application_context_switch_increase_percent": app_ctx_increase,
        "additional_peak_rss_bytes": extra_peak_rss,
    },
    "checks": checks,
    "passed": all(checks.values()),
    "raw_rounds": {"baseline": baseline, "ebpf_enabled": active},
}
with open(sys.argv[3], "w", encoding="utf-8") as stream:
    json.dump(report, stream, indent=2, allow_nan=False)
    stream.write("\n")
print(json.dumps(report, indent=2, allow_nan=False))
raise SystemExit(0 if report["passed"] else 1)
PY

"$cli" undeploy-app --state-file "$state_file" --app-id "$app_id"
restore_active=false
echo "eBPF overhead validation passed; evidence: $output"

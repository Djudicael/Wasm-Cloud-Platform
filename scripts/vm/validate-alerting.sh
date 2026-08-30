#!/usr/bin/env bash
# Validate every tracked alert expression and the local Alertmanager delivery path.

set -euo pipefail

state_file=.vm-testbed-state.json
output_file=

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --output) output_file=${2:?missing output path}; shift 2 ;;
    -h|--help)
      echo "Usage: validate-alerting.sh [--state-file PATH] [--output PATH]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"
command -v curl >/dev/null || { echo "curl is required." >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is required." >&2; exit 1; }
command -v podman >/dev/null || { echo "podman is required." >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 is required." >&2; exit 1; }

state_file=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$state_file")
services_file="${state_file}.services.json"
[[ -f "$state_file" && -f "$services_file" ]] || { echo "Missing topology/service state." >&2; exit 1; }

mapfile -t lifecycle < <(python3 - "$services_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    obs = (json.load(stream).get("observability") or {})
print(obs.get("state_key", ""))
print(obs.get("prometheus", ""))
print(obs.get("alertmanager", ""))
print(obs.get("alert_receiver", ""))
print(obs.get("alert_notification_log", ""))
for container in obs.get("containers", []):
    if container.get("name", "").endswith("-prometheus"):
        print(container.get("name", ""))
        print(container.get("id", ""))
        break
for container in obs.get("containers", []):
    if container.get("name", "").endswith("-alertmanager"):
        print(container.get("name", ""))
        print(container.get("id", ""))
        break
PY
)

state_key=${lifecycle[0]:-}
prometheus_url=${lifecycle[1]:-}
alertmanager_url=${lifecycle[2]:-}
alert_receiver_url=${lifecycle[3]:-}
notification_log=${lifecycle[4]:-}
prometheus_name=${lifecycle[5]:-}
prometheus_id=${lifecycle[6]:-}
alertmanager_name=${lifecycle[7]:-}
alertmanager_id=${lifecycle[8]:-}

[[ "$state_key" =~ ^[a-f0-9]{64}$ && "$prometheus_id" =~ ^[a-f0-9]{64}$ \
  && "$alertmanager_id" =~ ^[a-f0-9]{64}$ ]] || {
  echo "Invalid recorded observability identity." >&2
  exit 1
}
[[ "$prometheus_url" == http://127.0.0.1:9095 ]] || { echo "Unexpected Prometheus URL." >&2; exit 1; }
[[ "$alertmanager_url" == http://127.0.0.1:9093 ]] || { echo "Unexpected Alertmanager URL." >&2; exit 1; }
[[ "$alert_receiver_url" == http://127.0.0.1:19093 ]] || { echo "Alert receiver is not provisioned." >&2; exit 1; }
[[ "$notification_log" == /* ]] || { echo "Invalid notification log path." >&2; exit 1; }

inspected_name=$(podman inspect --format '{{.Name}}' "$prometheus_id")
inspected_label=$(podman inspect --format '{{index .Config.Labels "io.wasm-cloud-platform.state"}}' "$prometheus_id")
[[ "$inspected_name" == "$prometheus_name" && "$inspected_label" == "$state_key" ]] || {
  echo "Recorded Prometheus container identity mismatch." >&2
  exit 1
}
inspected_name=$(podman inspect --format '{{.Name}}' "$alertmanager_id")
inspected_label=$(podman inspect --format '{{index .Config.Labels "io.wasm-cloud-platform.state"}}' "$alertmanager_id")
[[ "$inspected_name" == "$alertmanager_name" && "$inspected_label" == "$state_key" ]] || {
  echo "Recorded Alertmanager container identity mismatch." >&2
  exit 1
}

prometheus_image=docker.io/prom/prometheus:v3.5.0
podman exec "$prometheus_id" promtool check config /etc/prometheus/prometheus.yml
podman exec "$alertmanager_id" amtool check-config /etc/alertmanager/alertmanager.yml
podman run --rm --entrypoint /bin/promtool \
  -v "$repo_root/deploy/prometheus:/rules:ro" -w /rules "$prometheus_image" \
  check rules admin_auth_alerts.yml platform_resource_alerts.yml \
  validation_alerts.yml wasi_policy_alerts.yml
podman run --rm --entrypoint /bin/promtool \
  -v "$repo_root/deploy/prometheus:/rules:ro" -w /rules "$prometheus_image" \
  test rules tests/alert_rules.test.yml

curl -fsS "$prometheus_url/-/ready" >/dev/null
curl -fsS "$alertmanager_url/-/ready" >/dev/null
curl -fsS "$alert_receiver_url/health" >/dev/null

temporary_dir=$(mktemp -d)
chmod 700 "$temporary_dir"
trap 'rm -rf -- "$temporary_dir"' EXIT
result_file="$temporary_dir/result.json"

python3 - "$prometheus_url" "$alertmanager_url" "$notification_log" "$result_file" <<'PY'
import datetime
import json
import os
import sys
import time
import urllib.parse
import urllib.request

prometheus_url, alertmanager_url, notification_log, result_file = sys.argv[1:]

expected_alerts = {
    "AdminAuthBruteForce", "AdminAuthNoToken", "AdminRateLimitActive",
    "AdminAuthDisabled", "AdminAuthSustainedFailures",
    "PlatformNodeDiskSpaceExhausted", "PlatformNodeInodesExhausted",
    "PlatformNodeMemoryPressure", "PlatformNodeFileDescriptorsHigh",
    "PlatformNodeDown", "ApplicationNotReady", "NatsExporterDown",
    "PostgreSQLExporterDown", "HAProxyExporterDown", "TelemetryCollectorDown",
    "TraceBackendDown", "PlatformHttpErrorRateHigh", "TelemetryExportFailures",
    "TelemetryQueueNearCapacity", "EbpfMonitoringUnavailable",
    "EbpfMonitoringIncomplete", "EbpfRingBufferEventsDropped",
    "EbpfDispatchQueueSaturated", "EbpfDropCounterUnavailable",
    "WasiPolicyConnectionDenied", "WasiPolicyFdExhaustion",
    "WasiPolicyEgressDenied", "WasiPolicyDnsDenied", "WasiPolicyBindDenied",
    "WasiPolicyFsWriteDenied", "WasiPolicyHighOutboundConnections",
    "WasiPolicyHighOpenFds",
}


def request_json(url, *, method="GET", payload=None):
    data = None
    headers = {}
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(request, timeout=10) as response:
        body = response.read()
    return json.loads(body) if body else None


rules_response = request_json(f"{prometheus_url}/api/v1/rules?type=alert")
if rules_response.get("status") != "success":
    raise SystemExit("Prometheus rule inventory failed")
rules = [rule for group in rules_response["data"]["groups"] for rule in group["rules"]]
names = {rule["name"] for rule in rules}
if names != expected_alerts:
    raise SystemExit(f"alert inventory mismatch: missing={sorted(expected_alerts-names)} extra={sorted(names-expected_alerts)}")

live_queries = []
for rule in sorted(rules, key=lambda item: item["name"]):
    query = urllib.parse.urlencode({"query": rule["query"]})
    response = request_json(f"{prometheus_url}/api/v1/query?{query}")
    if response.get("status") != "success" or response["data"].get("resultType") != "vector":
        raise SystemExit(f"live query failed for {rule['name']}")
    live_queries.append({"alert": rule["name"], "matching_series": len(response["data"]["result"])})

required_live_metrics = {
    "wasm_admin_auth_enabled", "wasm_admin_auth_failures_total",
    "wasm_admin_auth_successes_total", "wasm_admin_rate_limited_total",
    "wasm_node_disk_free_mb", "wasm_node_disk_free_inodes",
    "wasm_node_memory_usage_percent", "process_open_fds", "process_max_fds",
    "wasm_node_app_healthy_instances", "haproxy_backend_http_responses_total",
    "otelcol_exporter_send_failed_spans_total", "otelcol_exporter_queue_size",
    "otelcol_exporter_queue_capacity", "wasm_ebpf_monitoring_degraded",
    "wasm_ebpf_active", "wasm_ebpf_dispatch_queue_saturations_total",
    "wasm_policy_connection_denied_total", "wasm_policy_fd_denied_total",
    "wasm_policy_egress_denied_total", "wasm_policy_dns_denied_total",
    "wasm_policy_bind_denied_total", "wasm_policy_fs_write_denied_total",
    "wasm_policy_active_outbound_connections", "wasm_policy_open_fds",
}
metric_response = request_json(f"{prometheus_url}/api/v1/label/__name__/values")
live_metric_names = set(metric_response.get("data", []))
missing_metrics = sorted(required_live_metrics - live_metric_names)
if missing_metrics:
    raise SystemExit(f"alert source metrics absent from live Prometheus: {missing_metrics}")

# These labeled counter vectors intentionally have no series until the first
# matching fault. Their names and behavior are covered by promtool fixtures and
# the Phase 6 live fault tests; absence during a healthy final scrape is valid.
event_created_metrics = [
    "otelcol_exporter_enqueue_failed_spans_total",
    "wasm_ebpf_ring_buffer_dropped_events_total",
    "wasm_ebpf_ring_buffer_drop_counter_read_errors_total",
]


def notification_records(alert_names, instance):
    if not os.path.exists(notification_log):
        return []
    records = []
    with open(notification_log, encoding="utf-8") as stream:
        for line in stream:
            record = json.loads(line)
            alerts = record.get("payload", {}).get("alerts", [])
            if any(
                alert.get("labels", {}).get("alertname") in alert_names
                and alert.get("labels", {}).get("instance") == instance
                for alert in alerts
            ):
                records.append(record)
    return records


now = datetime.datetime.now(datetime.timezone.utc)
notification_alerts = [
    "PlatformNodeDown",
    "ApplicationNotReady",
    "PostgreSQLExporterDown",
    "NatsExporterDown",
    "PlatformHttpErrorRateHigh",
    "PlatformNodeDiskSpaceExhausted",
    "TelemetryExportFailures",
]
notification_instance = f"synthetic-validation-{int(now.timestamp())}"
starts_at = now.isoformat().replace("+00:00", "Z")
ends_at = (now + datetime.timedelta(minutes=30)).isoformat().replace("+00:00", "Z")
firing = [
    {
        "labels": {
            "alertname": alert_name,
            "cluster": "local-test",
            "instance": notification_instance,
            "severity": "warning",
        },
        "annotations": {"summary": "Synthetic P10-04 notification-path validation"},
        "startsAt": starts_at,
        "endsAt": ends_at,
    }
    for alert_name in notification_alerts
]
for _ in range(3):
    request_json(f"{alertmanager_url}/api/v2/alerts", method="POST", payload=firing)

deadline = time.monotonic() + 20
while time.monotonic() < deadline:
    records = notification_records(notification_alerts, notification_instance)
    firing_records = [record for record in records if record["payload"].get("status") == "firing"]
    delivered_names = {
        alert["labels"]["alertname"]
        for record in firing_records
        for alert in record["payload"].get("alerts", [])
        if alert.get("labels", {}).get("instance") == notification_instance
    }
    if delivered_names == set(notification_alerts):
        break
    time.sleep(1)
else:
    raise SystemExit("firing notification was not delivered")

time.sleep(6)
records = notification_records(notification_alerts, notification_instance)
firing_records = [record for record in records if record["payload"].get("status") == "firing"]
if len(firing_records) != len(notification_alerts):
    raise SystemExit(
        f"expected {len(notification_alerts)} deduplicated firing notifications, got {len(firing_records)}"
    )

resolved_at = datetime.datetime.now(datetime.timezone.utc)
resolved = []
for alert in firing:
    alert = dict(alert)
    alert["endsAt"] = resolved_at.isoformat().replace("+00:00", "Z")
    resolved.append(alert)
request_json(f"{alertmanager_url}/api/v2/alerts", method="POST", payload=resolved)

deadline = time.monotonic() + 25
while time.monotonic() < deadline:
    records = notification_records(notification_alerts, notification_instance)
    resolved_records = [record for record in records if record["payload"].get("status") == "resolved"]
    resolved_names = {
        alert["labels"]["alertname"]
        for record in resolved_records
        for alert in record["payload"].get("alerts", [])
        if alert.get("labels", {}).get("instance") == notification_instance
    }
    if resolved_names == set(notification_alerts):
        break
    time.sleep(1)
else:
    raise SystemExit("resolved notification was not delivered")

records = notification_records(notification_alerts, notification_instance)
if len(records) != 2 * len(notification_alerts):
    raise SystemExit(
        f"expected {2 * len(notification_alerts)} firing+resolved webhook deliveries, got {len(records)}"
    )

result = {
    "result": "pass",
    "rule_count": len(rules),
    "rule_names": sorted(names),
    "live_expression_queries": live_queries,
    "required_live_metrics_present": len(required_live_metrics),
    "event_created_metrics": event_created_metrics,
    "notification_test": {
        "alerts": notification_alerts,
        "identical_posts": 3,
        "firing_deliveries": len(notification_alerts),
        "resolved_deliveries": len(notification_alerts),
        "deduplicated": True,
    },
}
with open(result_file, "w", encoding="utf-8") as stream:
    json.dump(result, stream, indent=2)
    stream.write("\n")
PY

if [[ -n "$output_file" ]]; then
  output_file=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$output_file")
  mkdir -p "$(dirname "$output_file")"
  install -m 600 "$result_file" "$output_file"
fi

jq '{result,rule_count,required_live_metrics_present,notification_test}' "$result_file"

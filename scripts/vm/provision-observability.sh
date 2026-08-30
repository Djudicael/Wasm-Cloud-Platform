#!/usr/bin/env bash
# Add a disposable, state-scoped observability stack to a running testbed.

set -euo pipefail

state_file=.vm-testbed-state.json
metrics_token=${WASM_NODE_METRICS_TOKEN:-local-test-write-token-change-me}
replace=false

while (($#)); do
  case "$1" in
    --state-file) state_file=${2:?missing state path}; shift 2 ;;
    --metrics-token) metrics_token=${2:?missing metrics token}; shift 2 ;;
    --replace) replace=true; shift ;;
    -h|--help)
      echo "Usage: provision-observability.sh [--state-file PATH] [--metrics-token TOKEN] [--replace]"
      exit 0
      ;;
    *) echo "Unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ $(uname -s) == Linux ]] || { echo "Run this script in Linux or WSL2." >&2; exit 1; }
repo_root=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "Run inside the repository." >&2; exit 1; }
cd "$repo_root"
command -v podman >/dev/null || { echo "podman is required." >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 is required." >&2; exit 1; }
[[ -f "$state_file" ]] || { echo "Missing topology state: $state_file" >&2; exit 1; }

state_file=$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$state_file")
services_file="${state_file}.services.json"
[[ -f "$services_file" ]] || { echo "Missing service lifecycle state: $services_file" >&2; exit 1; }

state_key=$(printf '%s' "$state_file" | sha256sum | cut -d' ' -f1)
short_key=${state_key:0:12}
runtime_root="${XDG_RUNTIME_DIR:-/tmp}/wasm-cloud-platform-observability-$(id -u)"
runtime_dir="$runtime_root/$state_key"

mapfile -t node_records < <(python3 - "$state_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    state = json.load(stream)
for node in state.get("nodes", []):
    print(f'{node["id"]}\t{node["admin_addr"]}')
PY
)
[[ ${#node_records[@]} -gt 0 ]] || { echo "No platform nodes recorded." >&2; exit 1; }
node_ids=()
node_addrs=()
for record in "${node_records[@]}"; do
  IFS=$'\t' read -r node_id node_addr <<< "$record"
  [[ "$node_id" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "Unsafe node id in state: $node_id" >&2; exit 1; }
  node_ids+=("$node_id")
  node_addrs+=("$node_addr")
done

if python3 - "$services_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    raise SystemExit(0 if json.load(stream).get("observability") else 1)
PY
then
  if [[ "$replace" != true ]]; then
    echo "Observability lifecycle state already exists; use --replace after inspecting it." >&2
    exit 1
  fi
  mapfile -t existing_state < <(python3 - "$services_file" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as stream:
    observability = json.load(stream).get("observability") or {}
print(observability.get("state_key", ""))
print(observability.get("runtime_dir", ""))
for container in observability.get("containers", []):
    print(f'{container.get("name", "")}\t{container.get("id", "")}')
PY
  )
  [[ "${existing_state[0]:-}" == "$state_key" && "${existing_state[1]:-}" == "$runtime_dir" ]] || {
    echo "Recorded observability state does not match this state file; refusing replacement." >&2
    exit 1
  }
  for record in "${existing_state[@]:2}"; do
    IFS=$'\t' read -r container_name container_id <<< "$record"
    [[ -n "$container_name" && "$container_id" =~ ^[a-f0-9]{64}$ ]] || {
      echo "Invalid observability container record; refusing replacement." >&2
      exit 1
    }
    if podman container exists "$container_id"; then
      inspected_name=$(podman inspect --format '{{.Name}}' "$container_id")
      inspected_label=$(podman inspect --format '{{index .Config.Labels "io.wasm-cloud-platform.state"}}' "$container_id")
      [[ "$inspected_name" == "$container_name" && "$inspected_label" == "$state_key" ]] || {
        echo "Container $container_id does not match recorded state; refusing replacement." >&2
        exit 1
      }
      podman rm -f "$container_id" >/dev/null
    fi
  done
  if [[ -d "$runtime_dir" ]]; then
    [[ "$(python3 -c 'import os, sys; print(os.path.abspath(sys.argv[1]))' "$runtime_dir")" == "$runtime_root/$state_key" ]] || {
      echo "Observability runtime directory mismatch; refusing replacement." >&2
      exit 1
    }
    rm -rf -- "$runtime_dir"
  fi
  python3 - "$services_file" <<'PY'
import json, os, sys, tempfile
path = sys.argv[1]
with open(path, encoding="utf-8") as stream:
    state = json.load(stream)
state.pop("observability", None)
directory = os.path.dirname(path)
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
fi

mkdir -p "$runtime_dir/rules" "$runtime_dir/otel/storage" \
  "$runtime_dir/otel/export" "$runtime_dir/tempo/wal" "$runtime_dir/tempo/blocks"
chmod 700 "$runtime_root" "$runtime_dir"
chmod 700 "$runtime_dir/otel" "$runtime_dir/otel/storage" \
  "$runtime_dir/otel/export" "$runtime_dir/tempo" \
  "$runtime_dir/tempo/wal" "$runtime_dir/tempo/blocks"

printf '%s' "$metrics_token" > "$runtime_dir/metrics-token"
cat > "$runtime_dir/postgres.env" <<'EOF'
DATA_SOURCE_NAME=postgresql://oidc:oidc-local-test@172.20.0.20:5432/oidc?sslmode=disable&connect_timeout=2&options=-c%20statement_timeout%3D2000%20-c%20lock_timeout%3D1000%20-c%20idle_in_transaction_session_timeout%3D5000
EOF
chmod 600 "$runtime_dir/metrics-token" "$runtime_dir/postgres.env"
cp deploy/prometheus/admin_auth_alerts.yml \
  deploy/prometheus/platform_resource_alerts.yml \
  deploy/prometheus/wasi_policy_alerts.yml \
  "$runtime_dir/rules/"

cat > "$runtime_dir/rules/validation-alerts.yml" <<'EOF'
groups:
  - name: local_production_validation
    rules:
      - alert: PlatformNodeDown
        expr: up{job="platform-node"} == 0
        for: 10s
        labels: {severity: critical}
        annotations: {summary: "Platform node scrape is unavailable"}
      - alert: ApplicationNotReady
        expr: wasm_node_app_healthy_instances < 1
        for: 10s
        labels: {severity: critical}
        annotations: {summary: "A deployed application has no healthy instance"}
      - alert: NatsExporterDown
        expr: up{job="nats"} == 0
        for: 10s
        labels: {severity: critical}
        annotations: {summary: "NATS monitoring is unavailable"}
      - alert: PostgreSQLExporterDown
        expr: up{job="postgresql"} == 0
        for: 10s
        labels: {severity: critical}
        annotations: {summary: "PostgreSQL monitoring is unavailable"}
      - alert: HAProxyExporterDown
        expr: up{job="haproxy"} == 0
        for: 10s
        labels: {severity: warning}
        annotations: {summary: "HAProxy metrics are unavailable"}
      - alert: TelemetryCollectorDown
        expr: up{job="otel-collector"} == 0
        for: 10s
        labels: {severity: warning}
        annotations: {summary: "OpenTelemetry Collector is unavailable"}
      - alert: TraceBackendDown
        expr: up{job="tempo"} == 0
        for: 10s
        labels: {severity: warning}
        annotations: {summary: "The local trace query backend is unavailable"}
      - alert: TelemetryExportFailures
        expr: increase(otelcol_exporter_send_failed_spans_total[5m]) > 0 or increase(otelcol_exporter_enqueue_failed_spans_total[5m]) > 0
        labels: {severity: warning}
        annotations: {summary: "The telemetry pipeline failed to queue or export spans"}
      - alert: TelemetryQueueNearCapacity
        expr: otelcol_exporter_queue_size / clamp_min(otelcol_exporter_queue_capacity, 1) > 0.80
        for: 30s
        labels: {severity: warning}
        annotations: {summary: "The persistent telemetry export queue is near capacity"}
      - alert: EbpfMonitoringUnavailable
        expr: wasm_ebpf_monitoring_degraded == 1 and wasm_ebpf_active == 0
        for: 10s
        labels: {severity: warning}
        annotations: {summary: "Kernel eBPF monitoring is unavailable; the application node may still be serving"}
      - alert: EbpfMonitoringIncomplete
        expr: wasm_ebpf_monitoring_degraded == 1 and wasm_ebpf_active == 1
        for: 10s
        labels: {severity: warning}
        annotations: {summary: "One or more requested eBPF probes are unavailable"}
      - alert: EbpfRingBufferEventsDropped
        expr: increase(wasm_ebpf_ring_buffer_dropped_events_total[5m]) > 0
        labels: {severity: warning}
        annotations: {summary: "The kernel eBPF ring buffer dropped monitoring events"}
      - alert: EbpfDispatchQueueSaturated
        expr: increase(wasm_ebpf_dispatch_queue_saturations_total[5m]) > 0
        labels: {severity: warning}
        annotations: {summary: "The eBPF action-dispatch queue reached capacity"}
      - alert: EbpfDropCounterUnavailable
        expr: increase(wasm_ebpf_ring_buffer_drop_counter_read_errors_total[5m]) > 0
        labels: {severity: warning}
        annotations: {summary: "The node cannot read an eBPF ring-buffer drop counter"}
EOF

cat > "$runtime_dir/alertmanager.yml" <<'EOF'
route:
  receiver: local-validation
  group_by: [alertname, cluster, instance]
  group_wait: 1s
  group_interval: 5s
  repeat_interval: 1h
receivers:
  - name: local-validation
EOF

cat > "$runtime_dir/tempo.yml" <<'EOF'
server:
  http_listen_address: 127.0.0.1
  http_listen_port: 3200
  grpc_listen_address: 127.0.0.1
  grpc_listen_port: 14319
distributor:
  receivers:
    otlp:
      protocols:
        grpc:
          endpoint: 127.0.0.1:14317
ingester:
  max_block_duration: 1m
compactor:
  compaction:
    block_retention: 1h
storage:
  trace:
    backend: local
    wal:
      path: /var/tempo/wal
    local:
      path: /var/tempo/blocks
usage_report:
  reporting_enabled: false
EOF

cat > "$runtime_dir/otel-collector.yml" <<'EOF'
extensions:
  file_storage:
    directory: /var/lib/otelcol/storage
    create_directory: true
    timeout: 1s
    compaction:
      directory: /var/lib/otelcol/storage
      on_start: true
      on_rebound: true
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 172.20.0.1:4317
      http:
        endpoint: 172.20.0.1:4318
  filelog/platform:
    include: [/var/log/wcp/*.log]
    start_at: end
    storage: file_storage
    operators:
      - type: json_parser
        if: 'body matches "^\\{"'
processors:
  batch:
    send_batch_size: 256
    timeout: 1s
  memory_limiter:
    check_interval: 1s
    limit_mib: 192
    spike_limit_mib: 48
  resource/local_validation:
    attributes:
      - key: deployment.environment
        value: local-validation
        action: upsert
  filter/operational:
    error_mode: ignore
    logs:
      log_record:
        - 'attributes["target"] == "audit"'
  filter/audit:
    error_mode: ignore
    logs:
      log_record:
        - 'attributes["target"] != "audit"'
exporters:
  otlp/tempo:
    endpoint: 127.0.0.1:14317
    tls:
      insecure: true
    sending_queue:
      enabled: true
      num_consumers: 2
      queue_size: 2048
      storage: file_storage
    retry_on_failure:
      enabled: true
      initial_interval: 1s
      max_interval: 10s
      max_elapsed_time: 0s
  file/operational:
    path: /var/lib/otelcol/export/operational.json
    rotation:
      max_megabytes: 20
      max_days: 1
      max_backups: 5
    flush_interval: 1s
  file/audit:
    path: /var/lib/otelcol/export/audit.json
    rotation:
      max_megabytes: 20
      max_days: 7
      max_backups: 10
    flush_interval: 1s
service:
  extensions: [file_storage]
  telemetry:
    metrics:
      level: detailed
      readers:
        - pull:
            exporter:
              prometheus:
                host: 127.0.0.1
                port: 8888
  pipelines:
    traces:
      receivers: [otlp]
      processors: [memory_limiter, resource/local_validation, batch]
      exporters: [otlp/tempo]
    logs/operational:
      receivers: [filelog/platform, otlp]
      processors: [memory_limiter, resource/local_validation, filter/operational, batch]
      exporters: [file/operational]
    logs/audit:
      receivers: [filelog/platform, otlp]
      processors: [memory_limiter, resource/local_validation, filter/audit, batch]
      exporters: [file/audit]
EOF

{
  cat <<EOF
global:
  scrape_interval: 5s
  evaluation_interval: 5s
  external_labels:
    environment: local-production-validation
    cluster: single-host-microvm
rule_files:
  - /etc/prometheus/rules/*.yml
alerting:
  alertmanagers:
    - static_configs:
        - targets: [127.0.0.1:9093]
scrape_configs:
  - job_name: platform-node
    authorization:
      credentials_file: /etc/prometheus/metrics-token
    static_configs:
EOF
  index=0
  for address in "${node_addrs[@]}"; do
    cat <<EOF
      - targets: [$address]
        labels: {node: local-test-node-$index, role: platform}
EOF
    index=$((index + 1))
  done
  cat <<'EOF'
  - job_name: haproxy
    static_configs:
      - targets: [127.0.0.1:8405]
        labels: {role: front-door}
  - job_name: nats
    static_configs:
      - targets: [127.0.0.1:7777]
        labels: {role: messaging}
  - job_name: postgresql
    static_configs:
      - targets: [127.0.0.1:9187]
        labels: {role: database}
  - job_name: host
    static_configs:
      - targets: [127.0.0.1:9100]
        labels: {role: microvm-host}
  - job_name: otel-collector
    static_configs:
      - targets: [127.0.0.1:8888]
        labels: {role: telemetry}
  - job_name: tempo
    static_configs:
      - targets: [127.0.0.1:3200]
        labels: {role: tracing}
  - job_name: prometheus
    static_configs:
      - targets: [127.0.0.1:9095]
        labels: {role: monitoring}
  - job_name: alertmanager
    static_configs:
      - targets: [127.0.0.1:9093]
        labels: {role: alerting}
EOF
} > "$runtime_dir/prometheus.yml"
chmod 600 "$runtime_dir/prometheus.yml" "$runtime_dir/alertmanager.yml" \
  "$runtime_dir/otel-collector.yml" "$runtime_dir/tempo.yml"

containers=()
cleanup_failed_start() {
  for container in "${containers[@]}"; do
    podman rm -f "$container" >/dev/null 2>&1 || true
  done
}
trap cleanup_failed_start EXIT

run_container() {
  local role=$1 image=$2
  shift 2
  local -a run_options=()
  local -a command_arguments=()
  local command_section=false
  while (($#)); do
    if [[ "$1" == -- ]]; then
      command_section=true
    elif [[ "$command_section" == true ]]; then
      command_arguments+=("$1")
    else
      run_options+=("$1")
    fi
    shift
  done
  local name="wcp-obs-${short_key}-${role}"
  podman run -d --name "$name" \
    --label "io.wasm-cloud-platform.state=$state_key" \
    --network host "${run_options[@]}" "$image" "${command_arguments[@]}" >/dev/null
  containers+=("$name")
}

run_container node docker.io/natsio/prometheus-nats-exporter:0.17.3 \
  -- -varz -connz -healthz http://172.20.0.10:8222
run_container postgres quay.io/prometheuscommunity/postgres-exporter:v0.17.1 \
  --env-file "$runtime_dir/postgres.env" -- --web.listen-address=127.0.0.1:9187
run_container host quay.io/prometheus/node-exporter:v1.9.1 \
  --pid host -v /:/host:ro,rslave -- --path.rootfs=/host --web.listen-address=127.0.0.1:9100
run_container alertmanager docker.io/prom/alertmanager:v0.28.1 \
  --user 0 \
  -v "$runtime_dir/alertmanager.yml:/etc/alertmanager/alertmanager.yml:ro" \
  -- --config.file=/etc/alertmanager/alertmanager.yml --web.listen-address=127.0.0.1:9093
run_container tempo docker.io/grafana/tempo:2.10.7 \
  --user 0 \
  -v "$runtime_dir/tempo.yml:/etc/tempo.yml:ro" \
  -v "$runtime_dir/tempo:/var/tempo" \
  -- --config.file=/etc/tempo.yml
otel_mounts=(
  --user 0
  -v "$runtime_dir/otel-collector.yml:/etc/otelcol-contrib/config.yaml:ro"
  -v "$runtime_dir/otel:/var/lib/otelcol"
)
for node_id in "${node_ids[@]}"; do
  serial_log="/tmp/vm-testbed-${node_id}/serial.log"
  [[ -f "$serial_log" ]] || { echo "Missing serial log for $node_id: $serial_log" >&2; exit 1; }
  otel_mounts+=(-v "$serial_log:/var/log/wcp/${node_id}.log:ro")
done
run_container otel docker.io/otel/opentelemetry-collector-contrib:0.130.1 \
  "${otel_mounts[@]}" \
  -- --config=/etc/otelcol-contrib/config.yaml
run_container prometheus docker.io/prom/prometheus:v3.5.0 \
  --user 0 \
  -v "$runtime_dir/prometheus.yml:/etc/prometheus/prometheus.yml:ro" \
  -v "$runtime_dir/metrics-token:/etc/prometheus/metrics-token:ro" \
  -v "$runtime_dir/rules:/etc/prometheus/rules:ro" \
  -- \
  --config.file=/etc/prometheus/prometheus.yml \
  --storage.tsdb.path=/prometheus --storage.tsdb.retention.time=24h \
  --web.listen-address=127.0.0.1:9095

deadline=$((SECONDS + 60))
while ((SECONDS < deadline)); do
  if curl -fsS http://127.0.0.1:9095/-/ready >/dev/null \
    && curl -fsS http://127.0.0.1:9093/-/ready >/dev/null \
    && curl -fsS http://127.0.0.1:7777/metrics >/dev/null \
    && curl -fsS http://127.0.0.1:9187/metrics >/dev/null \
    && curl -fsS http://127.0.0.1:9100/metrics >/dev/null \
    && curl -fsS http://127.0.0.1:3200/ready >/dev/null \
    && curl -fsS http://127.0.0.1:8888/metrics >/dev/null; then
    break
  fi
  sleep 1
done
curl -fsS http://127.0.0.1:9095/-/ready >/dev/null || { echo "Prometheus did not become ready." >&2; exit 1; }
curl -fsS http://127.0.0.1:9093/-/ready >/dev/null || { echo "Alertmanager did not become ready." >&2; exit 1; }
curl -fsS http://127.0.0.1:7777/metrics >/dev/null || { echo "NATS exporter did not become ready." >&2; exit 1; }
curl -fsS http://127.0.0.1:9187/metrics >/dev/null || { echo "PostgreSQL exporter did not become ready." >&2; exit 1; }
curl -fsS http://127.0.0.1:9100/metrics >/dev/null || { echo "Host exporter did not become ready." >&2; exit 1; }
curl -fsS http://127.0.0.1:3200/ready >/dev/null || { echo "Tempo did not become ready." >&2; exit 1; }
curl -fsS http://127.0.0.1:8888/metrics >/dev/null || { echo "OpenTelemetry Collector metrics did not become ready." >&2; exit 1; }

python3 - "$services_file" "$runtime_dir" "$state_key" "${containers[@]}" <<'PY'
import json, os, subprocess, sys, tempfile
path, runtime_dir, state_key, *names = sys.argv[1:]
with open(path, encoding="utf-8") as stream:
    state = json.load(stream)
containers = []
for name in names:
    raw = subprocess.check_output(["podman", "inspect", name], text=True)
    inspected = json.loads(raw)[0]
    containers.append({
        "name": name,
        "id": inspected["Id"],
        "image": inspected["ImageName"],
    })
state["observability"] = {
    "type": "podman-local",
    "state_key": state_key,
    "runtime_dir": runtime_dir,
    "containers": containers,
    "prometheus": "http://127.0.0.1:9095",
    "alertmanager": "http://127.0.0.1:9093",
    "otel_grpc": "http://172.20.0.1:4317",
    "tempo": "http://127.0.0.1:3200",
    "operational_log": os.path.join(runtime_dir, "otel", "export", "operational.json"),
    "audit_log": os.path.join(runtime_dir, "otel", "export", "audit.json"),
}
directory = os.path.dirname(path)
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

trap - EXIT
echo "Prometheus ready at http://127.0.0.1:9095"
echo "Alertmanager ready at http://127.0.0.1:9093"
echo "OpenTelemetry OTLP receiver ready for microVMs at http://172.20.0.1:4317"
echo "Tempo query API ready at http://127.0.0.1:3200"

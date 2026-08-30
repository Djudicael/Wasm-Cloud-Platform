# Observability & Monitoring

This guide covers the complete observability stack of the Wasm Cloud Platform — metrics, logging, distributed tracing, eBPF-based kernel monitoring, health checks, and alerting.

For the dedicated eBPF kernel monitoring and security guide, see [`docs/ebpf.md`](ebpf.md).

## Table of Contents

1. [Overview](#overview)
2. [Metrics (Prometheus)](#metrics-prometheus)
3. [Logging](#logging)
4. [Distributed Tracing](#distributed-tracing)
5. [eBPF Kernel Monitoring](#ebpf-kernel-monitoring)
6. [Health Checks](#health-checks)
7. [Alerting](#alerting)
8. [Grafana Dashboards](#grafana-dashboards)
9. [SRE Playbooks](#sre-playbooks)
10. [Troubleshooting Guide](#troubleshooting-guide)

---

## Overview

The platform produces observability data at every layer:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                           Observability Stack                                │
│                                                                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐   │
│  │   Metrics    │  │    Logs      │  │    Traces    │  │   eBPF       │   │
│  │  Prometheus  │  │  JSON/Text   │  │  OpenTelemetry│  │  Kernel      │   │
│  │              │  │              │  │              │  │  Events      │   │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘   │
│         │                 │                 │                 │           │
│         ▼                 ▼                 ▼                 ▼           │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │                    Grafana (Visualization)                           │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│         ▲                 ▲                 ▲                 ▲           │
│         │                 │                 │                 │           │
│  ┌──────┴───────┐  ┌──────┴───────┐  ┌──────┴───────┐  ┌──────┴───────┐   │
│  │ Alertmanager │  │  Loki/ES     │  │   Tempo/     │  │  eBPF        │   │
│  │              │  │              │  │   Jaeger     │  │  Alerts      │   │
│  └──────────────┘  └──────────────┘  └──────────────┘  └──────────────┘   │
└─────────────────────────────────────────────────────────────────────────────┘
```

### What is observed

| Layer | What | How |
|-------|------|-----|
| **Proxy (Pingora)** | Request rates, latencies, 4xx/5xx, rate limit rejections, circuit breaker state | Prometheus counters/gauges |
| **Gateway** | Auth success/failure, CORS preflights, JWT validation errors | Prometheus counters |
| **Supervisor** | Instance count, spawn/kill events, fuel consumption, memory usage | Prometheus + logs |
| **Wasm Runtime** | Fuel consumed per request, memory growth, trap rates | Prometheus + logs |
| **Storage (redb)** | Read/write latency, backup events, migration status | Prometheus + logs |
| **NATS** | Connection status, message throughput, JetStream lag | Prometheus + logs |
| **eBPF** | Syscall anomalies, memory pressure, FD exhaustion, TCP retransmits | Kernel events + metrics |
| **Host** | CPU, memory, disk I/O, network I/O | Node exporter |

---

## Metrics (Prometheus)

The platform exposes a Prometheus scrape endpoint on every node.

### Scrape endpoint

```
GET http://<node>:9090/metrics
```

### Key metrics

#### Request metrics (proxy)

| Metric | Type | Description |
|--------|------|-------------|
| `wasm_proxy_requests_total` | Counter | Total HTTP requests by app, status, method |
| `wasm_proxy_request_duration_seconds` | Histogram | Request latency by app |
| `wasm_proxy_upstream_errors_total` | Counter | Failed upstream connections |
| `wasm_proxy_active_connections` | Gauge | Current active client connections |

#### Rate limiting metrics

| Metric | Type | Description |
|--------|------|-------------|
| `wasm_rate_limit_rejections_total` | Counter | Rejected requests by app, reason |
| `wasm_rate_limit_allowed_total` | Counter | Allowed requests by app |

#### Gateway metrics

| Metric | Type | Description |
|--------|------|-------------|
| `wasm_gateway_auth_success_total` | Counter | Successful authentications |
| `wasm_gateway_auth_failure_total` | Counter | Failed authentications |
| `wasm_gateway_authz_denied_total` | Counter | Authorization denials (403) |
| `wasm_gateway_rate_limit_denied_total` | Counter | Rate limit rejections |
| `wasm_gateway_circuit_breaker_rejected_total` | Counter | Circuit breaker rejections |
| `wasm_gateway_cors_preflight_total` | Counter | CORS preflight requests handled |
| `wasm_gateway_circuits_open` | Gauge | Number of open circuits |

#### Supervisor metrics

| Metric | Type | Description |
|--------|------|-------------|
| `wasm_supervisor_instances` | Gauge | Running instances by app |
| `wasm_supervisor_spawn_total` | Counter | Instance spawn events |
| `wasm_supervisor_kill_total` | Counter | Instance kill events |
| `wasm_supervisor_fuel_consumed_total` | Counter | Total fuel consumed by app |
| `wasm_supervisor_memory_bytes` | Gauge | Memory usage by instance |

#### Billing metrics

| Metric | Type | Description |
|--------|------|-------------|
| `wasm_billing_fuel_consumed` | Counter | Fuel per app per tenant |
| `wasm_billing_wall_clock_ms` | Counter | Wall-clock time per instance |

#### eBPF metrics

| Metric | Type | Description |
|--------|------|-------------|
| `wasm_ebpf_oom_kills_total` | Counter | OOM kill events |
| `wasm_ebpf_process_exits_total` | Counter | Process exit events |
| `wasm_ebpf_tcp_retransmits_total` | Counter | TCP retransmit events |
| `wasm_ebpf_security_violations_total` | Counter | Syscall anomaly events |
| `wasm_ebpf_fd_usage_ratio` | Gauge | FD usage as ratio |
| `wasm_ebpf_memory_pressure` | Gauge | Memory pressure level (0–3) |

### Prometheus configuration

```yaml
# /etc/prometheus/prometheus.yml
global:
  scrape_interval: 15s
  evaluation_interval: 15s

scrape_configs:
  # Platform nodes
  - job_name: 'wasm-nodes'
    static_configs:
      - targets:
        - 'node-1:9090'
        - 'node-2:9090'
        - 'node-3:9090'
    metrics_path: /metrics

  # NATS servers
  - job_name: 'nats'
    static_configs:
      - targets:
        - 'nats-1:8222'
        - 'nats-2:8222'
        - 'nats-3:8222'

  # Host metrics (node_exporter)
  - job_name: 'node'
    static_configs:
      - targets:
        - 'node-1:9100'
        - 'node-2:9100'
        - 'node-3:9100'
```

### Query examples

```promql
# Request rate by app
sum(rate(wasm_proxy_requests_total[5m])) by (app_id)

# 95th percentile latency
histogram_quantile(0.95, 
  sum(rate(wasm_proxy_request_duration_seconds_bucket[5m])) by (le, app_id)
)

# Error rate
sum(rate(wasm_proxy_requests_total{status=~"5.."}[5m])) by (app_id)

# Open circuit breakers
wasm_gateway_circuits_open

# Instances per node
sum(wasm_supervisor_instances) by (node_id)

# Fuel consumption rate
sum(rate(wasm_supervisor_fuel_consumed_total[5m])) by (app_id)

# eBPF security violations
sum(rate(wasm_ebpf_security_violations_total[5m])) by (node_id)
```

### Operational Monitoring Queries

```promql
# Nodes that have restarted recently (uptime < 5 minutes)
wasm_node_uptime_seconds < 300

# Apps with zero healthy instances
sum(wasm_supervisor_instances{state="healthy"}) by (app_id) == 0

# Cold start rate (instances spawned without idle pool)
sum(rate(wasm_supervisor_spawn_total{cold_start="true"}[5m])) by (app_id)

# Request queue depth (waiting for instance)
sum(wasm_proxy_request_queue_depth) by (app_id)

# NATS control-plane lag per node
sum(nats_consumer_pending_messages) by (node_id)

# Disk usage trend (predict full in 24h)
predict_linear(node_filesystem_avail_bytes{mountpoint="/var/lib/wasm-node"}[1h], 86400) < 0

# Memory leak detection (memory grows over 1h)
deriv(wasm_supervisor_memory_bytes[1h]) > 1000000

# Zombie instances (reported but not accepting connections)
(wasm_supervisor_instances - wasm_proxy_healthy_upstreams) > 0
```

### Metric Retention

| Storage | Retention | Granularity |
|---------|-----------|-------------|
| Prometheus (raw) | 15 days | 15s |
| Prometheus (downsampled) | 1 year | 5m |
| redb ( MetricBucket) | 7 days | 1min |
| redb (audit log) | 90 days | event |
| Loki / Elasticsearch | 30 days | line |
| Tempo / Jaeger | 7 days | trace |

---

## Logging

The platform uses structured logging with `tracing`.

### Log formats

```bash
# JSON (default for production)
wasm-node --log-format json

# Text (human-readable for development)
wasm-node --log-format text

# Output to file
wasm-node --log-output /var/log/wasm-node/app.log
```

### JSON log format

```json
{
  "timestamp": "2026-04-25T10:30:00.123Z",
  "level": "INFO",
  "fields": {
    "message": "instance ready",
    "app_id": "api-users:v2",
    "addr": "127.0.0.1:10101",
    "instance_id": "550e8400-e29b-41d4-a716-446655440000"
  },
  "target": "supervisor",
  "span": {
    "name": "spawn_instance",
    "app_id": "api-users:v2"
  },
  "spans": [
    { "name": "spawn_instance" }
  ]
}
```

### Log levels

| Level | What gets logged |
|-------|-----------------|
| `ERROR` | Traps, failed spawns, storage errors, auth failures |
| `WARN` | Rate limit hits, circuit breaker opens, eBPF pressure |
| `INFO` | Deployments, instance lifecycle, route changes, health status |
| `DEBUG` | Request routing decisions, policy checks, JWKS refresh |
| `TRACE` | Per-request details, WASI syscall traces |

### Changing log levels at runtime

```bash
# View current levels
wasm-ctl node config

# Set new levels (hot-reload)
wasm-ctl node config --set logging_level=debug

# Or via the admin API
curl -X PATCH http://node:9090/admin/config \
  -H "Content-Type: application/json" \
  -d '{"logging_level": "debug"}'
```

### Log forwarding

The platform can forward logs to external systems:

```toml
[logging.forward]
buffer_capacity = 8192
batch_size = 200
flush_interval_ms = 1000

[[logging.forward.sinks]]
type = "elasticsearch"
endpoint = "https://logs.example.com:9200"
index_prefix = "wasm-logs"

[[logging.forward.sinks]]
type = "loki"
endpoint = "https://loki.example.com:3100"

[[logging.forward.sinks]]
type = "nats"
subject = "logs.platform.>"
```

---

## Distributed Tracing

The platform propagates trace contexts across services using OpenTelemetry (W3C traceparent).

### Trace propagation

**Incoming request:**
```
GET /api/orders HTTP/1.1
Host: shop.example.com
traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01
```

**Forwarded to upstream:**
```
GET /api/orders HTTP/1.1
Host: 127.0.0.1:10101
X-Trace-Id: 4bf92f3577b34da6a3ce929d0e0e4736
traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01
```

### Span structure

```
Trace: 4bf92f3577b34da6a3ce929d0e0e4736
└── [proxy] http.server.request
    service.name=wasm-node
    service.instance.id=<node-id>
    application.id=<exact-deployment-id>
    http.request.method=GET
    url.path=/api/orders
    http.response.status_code=200
    http.server.request.duration_ms=45
```

The platform currently emits one server span around proxy routing and WASI
execution. Application logs created while handling that request inherit the
same trace context. Child spans for database queries or internal calls exist
only when the application or its client library explicitly instruments them;
the platform does not invent those spans.

### OTLP export

```bash
# Configure the node's local OTLP agent endpoint
# config.toml
[logging]
otlp_endpoint = "http://127.0.0.1:4317"
```

Each host should run a supervised local Collector with a bounded disk-backed
export queue. The node's Rust batch exporter is bounded in memory, but it is
not a write-ahead log: a span can be lost if the immediate Collector is down.
Use authenticated TLS outside the isolated local test network. Trace resources
include `service.name`, `service.version`, and `service.instance.id`; the local
validation Collector adds `deployment.environment`.

The complete production contract and interruption tests are in
[`INFRA_IMPL/process/PRODUCTION_TELEMETRY_VALIDATION.md`](../INFRA_IMPL/process/PRODUCTION_TELEMETRY_VALIDATION.md).

### Trace IDs in logs

Every log line includes the trace ID when available:

```json
{
  "timestamp": "2026-04-25T10:30:00.123Z",
  "level": "INFO",
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
  "fields": {
    "message": "request completed",
    "latency_ms": 45,
    "status": 200
  }
}
```

---

## eBPF Kernel Monitoring

On Linux with kernel 5.8+ and BTF, the platform uses eBPF programs for kernel-level observability.

### What eBPF monitors

| Subsystem | Event | Action |
|-----------|-------|--------|
| **Memory** | OOM kill | Log + activate backpressure |
| **Process** | Process exit | Log + notify supervisor |
| **TCP** | Retransmit spike | Log + mark NATS degraded |
| **Syscall** | Privileged syscall | Log security incident + alert |
| **FD** | FD exhaustion | Prune idle instances |
| **Disk** | Slow I/O (>50ms) | Log + alert |

### Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     Linux Kernel                             │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │  eBPF Prog   │  │  eBPF Prog   │  │  eBPF Prog   │      │
│  │  (OOM)       │  │  (TCP)       │  │  (Syscall)   │      │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘      │
│         │                 │                 │              │
│         └─────────────────┴─────────────────┘              │
│                           │                                │
│                           ▼                                │
│                    ┌─────────────┐                         │
│                    │   perfbuf   │                         │
│                    │   / ring    │                         │
│                    └──────┬──────┘                         │
└───────────────────────────┼─────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────┐
│              Platform Node (userspace)                       │
│                                                              │
│  ┌─────────────────┐    ┌─────────────────┐                │
│  │  eBPF Loader    │───►│  Event Parser   │                │
│  │  (aya-rs)       │    │                 │                │
│  └─────────────────┘    └────────┬────────┘                │
│                                   │                         │
│                    ┌──────────────┼──────────────┐         │
│                    ▼              ▼              ▼         │
│             ┌──────────┐  ┌──────────┐  ┌──────────┐     │
│             │ Metrics  │  │  Logs    │  │ Actions  │     │
│             │ Registry │  │          │  │          │     │
│             └──────────┘  └──────────┘  └──────────┘     │
│                                              │            │
│                                              ▼            │
│                                    ┌───────────────────┐  │
│                                    │ ActionDispatcher  │  │
│                                    │                   │  │
│                                    │ • Backpressure    │  │
│                                    │ • Kill instance   │  │
│                                    │ • Prune idle      │  │
│                                    │ • Mark NATS down  │  │
│                                    └───────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

### Activation

```bash
# Check if eBPF is active
wasm-ctl node ebpf-status

# Example output:
# Mode:              eBPF (kernel)
# Backpressure:      normal
# Degraded Mode:     no
# Pressure Level:    none
# OOM Kills:         0
# Process Exits:     12
# TCP Retransmits:   3
# Security Violations: 0
# Events Processed:  1048576
# Parse Errors:      0
```

### Fallback mode

If the kernel does not support eBPF (older kernel, missing BTF, or non-Linux), the monitor falls back to **userspace polling** with a 5-second interval.

### Sending commands to eBPF

```bash
# Prune idle instances to free FDs
wasm-ctl node ebpf-config --prune-idle --idle-threshold-secs 60

# Kill the largest instance (memory pressure recovery)
wasm-ctl node ebpf-config --kill-largest --kill-largest-reason "memory pressure"
```

### eBPF configuration

```toml
[ebpf]
enabled = true
fd_soft_limit = 8192
fd_hard_limit = 9728
mem_low_threshold_pages = 65536
mem_critical_threshold_pages = 16384
disk_slow_threshold_ns = 50000000
tcp_conn_limit_per_pid = 10000
syscall_rate_limit = 100000
sampling_period_secs = 10
```

### Security incident handling

When eBPF detects a suspicious syscall:

```
1. Kernel eBPF probe fires on syscall entry
2. Check if syscall is in allowlist
3. If not, record: pid, syscall_nr, app_id (if known)
4. Send event to userspace via perf buffer
5. Platform publishes SecurityIncident NATS event
6. Log: "eBPF: security incident detected"
7. Metrics: wasm_ebpf_security_violations_total +1
```

---

## Health Checks

The platform exposes multiple health endpoints for orchestrators and load balancers.

### Endpoints

| Endpoint | Purpose | Checks |
|----------|---------|--------|
| `GET /livez` | Liveness probe | Node process is running |
| `GET /readyz` | Readiness probe | All dependencies healthy |
| `GET /healthz` | Health check | Node is accepting requests |
| `GET /status` | Full status | Detailed health report with dependencies |

### Response format

```bash
curl http://node:9090/readyz
```

```json
{
  "status": "healthy",
  "node_id": "node-1",
  "timestamp": "2026-04-25T10:30:00Z",
  "uptime_secs": 86400,
  "startup_complete": true,
  "accepting_requests": true,
  "active_instances": 42,
  "deployed_apps": 8,
  "dependencies": [
    {
      "name": "nats",
      "status": "healthy",
      "message": "connected",
      "latency_ms": 2,
      "last_check": "2026-04-25T10:29:55Z"
    },
    {
      "name": "redb",
      "status": "healthy",
      "message": "writable",
      "latency_ms": 1,
      "last_check": "2026-04-25T10:29:55Z"
    },
    {
      "name": "disk",
      "status": "healthy",
      "message": "1.2TB free",
      "latency_ms": 0,
      "last_check": "2026-04-25T10:29:55Z"
    },
    {
      "name": "memory",
      "status": "healthy",
      "message": "4.2GB used / 32GB total",
      "latency_ms": 0,
      "last_check": "2026-04-25T10:29:55Z"
    },
    {
      "name": "backpressure",
      "status": "healthy",
      "message": "accepting requests",
      "latency_ms": 0,
      "last_check": "2026-04-25T10:29:55Z"
    }
  ],
  "apps": [
    {
      "app_id": "api-users:v2",
      "instances": 5,
      "healthy_instances": 5,
      "serving": true
    }
  ]
}
```

### Kubernetes probes

```yaml
apiVersion: v1
kind: Pod
spec:
  containers:
    - name: wasm-node
      image: wasm-node:latest
      livenessProbe:
        httpGet:
          path: /livez
          port: 9090
        initialDelaySeconds: 30
        periodSeconds: 10
      readinessProbe:
        httpGet:
          path: /readyz
          port: 9090
        initialDelaySeconds: 5
        periodSeconds: 5
```

### Health state transitions

```
STARTING → HEALTHY → DEGRADED → UNHEALTHY
    │          │          │           │
    │          │          │           └── Node should be removed from LB
    │          │          └── One or more dependencies failing
    │          └── All dependencies healthy, accepting requests
    └── Node has not finished startup sequence yet
```

---

## Alerting

### Prometheus and Alertmanager rules

The deployed rule definitions are the four YAML files under
`deploy/prometheus/`. Do not copy alert names or metric names from an
illustrative dashboard: the tracked files are the contract.

The current set contains 32 rules covering admin authentication, platform
resources and availability, HAProxy HTTP errors, telemetry, eBPF degraded/loss
modes, and WASI policy enforcement. All expressions have representative
threshold inputs in `deploy/prometheus/tests/alert_rules.test.yml`.

```bash
podman run --rm --entrypoint /bin/promtool \
  -v "$PWD/deploy/prometheus:/rules:ro" -w /rules \
  docker.io/prom/prometheus:v3.5.0 \
  test rules tests/alert_rules.test.yml

bash scripts/vm/validate-alerting.sh \
  --state-file .prod-validation-single-host-state.json
```

The live validator checks both configurations, inventories all 32 rules,
executes every expression against Prometheus, verifies always-present source
metrics, and proves Alertmanager firing, resolution, and deduplication through
the state-scoped test receiver. See the
[production alerting validation runbook](../INFRA_IMPL/process/PRODUCTION_ALERTING_VALIDATION.md).

### Notification channels

The local validation receiver writes protected JSONL evidence and is never a
production destination. Production must configure an authenticated operator
receiver such as PagerDuty, Opsgenie, a ticketing system, or an internal webhook.
Keep credentials outside the configuration file, use TLS, assign each alert an
owner and reachable runbook, and test grouping, inhibition, repetition, firing,
resolution, and receiver failure in staging.

---

## Grafana Dashboards

### Dashboard: Platform Overview

**Row 1: Request Traffic**
- Request rate (req/s) by app
- Error rate (%) by app
- P50/P95/P99 latency by app

**Row 2: Instance Health**
- Running instances by app
- Instance spawn/kill rate
- Cold start count

**Row 3: Gateway**
- Auth success/failure rate
- Rate limit rejections
- Open circuit breakers
- CORS preflight rate

**Row 4: Resources**
- Fuel consumption rate
- Memory usage per instance
- Fuel vs wall-clock ratio

**Row 5: eBPF / Kernel**
- OOM kills
- Security violations
- Memory pressure level
- FD usage ratio

**Row 6: Dependencies**
- NATS connection status
- redb latency
- Disk free space
- Node memory

### Dashboard: Per-App Detail

- Request volume and errors
- Instance scaling (min/max/current)
- Gateway config state (auth, rate limit, circuit)
- Fuel consumption per request
- Upstream health

---

## SRE Playbooks

### Playbook: Node Joins Cluster but Never Receives Deploys

**Symptoms**: Node shows healthy, `/readyz` passes, but no apps are deployed.

**Diagnosis**:
```bash
# Check NATS consumer lag
wasm-ctl node health | jq '.dependencies[] | select(.name == "nats")'

# Check JetStream consumer info
nats consumer info DEPLOY_EVENTS wasm-node-<node_id>

# Verify node is in routing table
wasm-ctl nodes
```

**Resolution**:
1. If NATS consumer lag > 1000: Restart node to recreate consumer
2. If node not in routing table: Check `node_id` uniqueness, restart with new ID
3. If NATS shows connected but no messages: Check subject filters match deployment events

### Playbook: Rolling Upgrade Stuck at 50%

**Symptoms**: `wasm-ctl upgrade status` shows "in_progress" for >10 minutes.

**Diagnosis**:
```bash
# Check which nodes haven't acked
wasm-ctl upgrade status --json | jq '.pending_nodes'

# Check those nodes' health
for node in $(wasm-ctl upgrade status --json | jq -r '.pending_nodes[]'); do
  curl -s http://$node:9090/readyz | jq '.status'
done
```

**Resolution**:
1. If node is UNHEALTHY: Remove from LB, investigate separately
2. If node is DEGRADED: Wait for recovery, or force-ack with `wasm-ctl upgrade ack --node <id>`
3. If node is healthy but not acking: Check NATS subject `upgrade.ack.>` for dropped messages

### Playbook: Fuel Billing Discrepancy

**Symptoms**: Billing metrics show 2x expected fuel for an app.

**Diagnosis**:
```bash
# Compare fuel metrics from multiple sources
curl http://node:9090/metrics | grep 'wasm_billing_fuel_consumed{app_id="api-users"}'
curl http://node:9090/metrics | grep 'wasm_supervisor_fuel_consumed_total{app_id="api-users"}'

# Check for duplicate instances
wasm-ctl instances --app api-users:v2

# Verify no metric double-counting from restarts
curl http://node:9090/metrics | grep 'wasm_node_start_time'
```

**Resolution**:
1. If duplicate instances: Kill stale instances, check supervisor health loop
2. If metric sources diverge: The billing counter is authoritative (tamper-evident chain)
3. If node restarted: Expected behavior — counters reset, billing uses chain-verified values

### Playbook: Intermittent 502 Errors After Deploy

**Symptoms**: App deployed successfully, but ~1% of requests return 502.

**Diagnosis**:
```bash
# Check if upstream table has stale instances
curl http://node:9090/admin/upstream | jq '.instances[] | select(.app_id == "api-users:v2")'

# Check instance health
wasm-ctl instances --app api-users:v2 --health

# Look for pre-emptive removals in logs
grep "preemptive_remove" /var/log/wasm-node/supervisor.log | tail -20
```

**Resolution**:
1. If stale instances in table: eBPF detected exits faster than health loop — this is expected, gaps close in <5s
2. If instances are crashing: Check `wasm_supervisor_kill_total{reason="trap"}`
3. If consistent 502s: Verify app binds to correct port, check WASI config

### Playbook: Platform-Wide Latency Spike

**Symptoms**: All apps show p99 latency >1s simultaneously.

**Diagnosis**:
```bash
# Check node-level resources
for node in node-{1,2,3}; do
  echo "=== $node ==="
  curl -s http://$node:9090/metrics | grep -E "memory_pressure|backpressure|disk_slow"
done

# Check NATS cluster health
nats server report connections
nats server report accounts

# Check for network partition
nats server report gateways
```

**Resolution**:
1. If memory pressure on all nodes: Global traffic spike — add nodes, enable backpressure
2. If disk slow on all nodes: Shared storage degraded — switch to local disk mode
3. If NATS partition: Restart NATS cluster, check network switches
4. If no obvious cause: Check for platform-wide rate limiting or circuit breaker misconfiguration

---

## Troubleshooting Guide

### Symptom: High latency on all requests

**Check:**
```bash
# Is the node overloaded?
wasm-ctl node health

# Are instances being killed frequently?
curl http://node:9090/metrics | grep wasm_supervisor_kill_total

# Is backpressure active?
curl http://node:9090/metrics | grep backpressure
```

**Likely causes:**
1. Node CPU saturated → Add nodes or reduce `max_instances`
2. Memory pressure → eBPF triggered, instances killed
3. Circuit breaker flapping → Check upstream health
4. Cold starts happening → Increase `idle_timeout_secs`

### Symptom: 401 errors on all requests

**Check:**
```bash
# Is OIDC provider reachable?
curl <issuer_url>/.well-known/openid-configuration

# Is JWKS cached?
wasm-ctl node config | grep jwks

# Check auth metrics
curl http://node:9090/metrics | grep wasm_gateway_auth_failure
```

**Likely causes:**
1. Keycloak/realms down → Check OIDC provider
2. Clock skew > 30s → Sync NTP
3. Wrong audience → Verify `audience` in config
4. Expired JWKS cache → Check `jwks_refresh_secs`

### Symptom: 429 errors (rate limited)

**Check:**
```bash
# Current rate limit config
wasm-ctl gateway show <app_id>

# Rate limit metrics
curl http://node:9090/metrics | grep wasm_rate_limit_rejections
```

**Fix:**
```bash
# Increase rate limit
wasm-ctl gateway set-rate-limit <app_id> \
  --rps 1000 \
  --burst 200 \
  --distributed
```

### Symptom: eBPF security incidents firing

**Check:**
```bash
# View recent incidents
wasm-ctl node ebpf-status

# Check which syscalls triggered
grep "security incident" /var/log/wasm-node/audit.jsonl | tail -20
```

**Action:**
1. Identify the app and instance
2. Check if it was a false positive (legitimate syscall)
3. Update the WASI policy allowlist if needed
4. Kill the instance if malicious

### Symptom: Node shows DEGRADED but not UNHEALTHY

**Check:**
```bash
# Which dependency is failing?
wasm-ctl node health
# or
curl http://node:9090/readyz | jq '.dependencies'
```

**Likely causes:**
- NATS disconnected → Check network, restart NATS
- Disk full → Run GC, delete old artifacts
- Memory pressure → Kill largest instance, add RAM

---

## Quick Reference

| Task | Command |
|------|---------|
| View node health | `wasm-ctl node health` |
| View eBPF status | `wasm-ctl node ebpf-status` |
| Prune idle instances | `wasm-ctl node ebpf-config --prune-idle` |
| Kill largest instance | `wasm-ctl node ebpf-config --kill-largest` |
| Get metrics | `curl http://node:9090/metrics` |
| Full status | `curl http://node:9090/status` |
| Liveness | `curl http://node:9090/livez` |
| Readiness | `curl http://node:9090/readyz` |
| Change log level | `wasm-ctl node config --set logging_level=debug` |
| View gateway config | `wasm-ctl gateway show <app_id>` |
| View app list | `wasm-ctl app list` |
| View instances | `wasm-ctl instances` |
| Stream logs | `wasm-ctl logs <app_id>` |

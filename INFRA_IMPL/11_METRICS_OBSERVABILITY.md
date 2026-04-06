# Step 11 — Metrics & Observability

## Goal
Implement the full observability stack. The Supervisor emits metrics after every Wasm execution.
These metrics are:
1. Buffered in RAM (non-blocking on the hot path)
2. Aggregated per minute and written to `redb`
3. Exposed via a Prometheus-compatible `/metrics` endpoint
4. Optionally exported via OpenTelemetry to Jaeger/Grafana Tempo for distributed tracing

---

## Context & Rationale

### The Problem This Solves

Without metrics, the platform is a black box. Operators cannot answer:
- Is the `api-users` app running out of fuel (hitting its quota)?
- What is the p99 latency for `payments:v2`?
- How many requests are failing with 500?
- Is a specific app leaking memory (RAM grows over time)?

This step makes those questions answerable in real time, using the standard tooling the
industry already uses: Prometheus for metrics scraping, Grafana for dashboards, and
OpenTelemetry for distributed traces.

### Why the Two-Layer Approach (RAM Buffer → redb)?

Metrics are emitted on **every HTTP request** — potentially thousands per second per node.
Writing directly to `redb` on every request would serialize all request handling through
a single write lock. This would be a catastrophic performance bottleneck.

The two-layer approach solves this:

```
Request completes
      │
      ▼ (non-blocking, try_send)
mpsc channel (capacity 10,000)   ← never blocks the request handler
      │
      ▼ (background task, every 60s)
Aggregate N samples → 1 MetricBucket
      │
      ▼ (one write per minute per app)
redb [metrics table]             ← minimal write pressure
```

The `try_send` on the channel means: if the channel is full (extremely heavy load), the
sample is **dropped with a warning** rather than blocking the request. This is the correct
tradeoff — dropping some metrics under extreme load is better than slowing down actual
user traffic.

### Why Prometheus (not InfluxDB, Datadog)?

Prometheus uses a **pull model**: the Prometheus server periodically scrapes `/metrics`
from each node. This has key advantages for this platform:

1. **Zero push configuration**: nodes don't need to know where to send metrics — they just
   expose an endpoint
2. **Industry standard**: every operator already knows how to set up Prometheus + Grafana
3. **Local scraping**: metrics are always available locally, even during network partitions

The `prometheus` crate implements the text format natively in Rust. No daemon, no agent.

### Why OpenTelemetry for Tracing (not Just Logs)?

Logs tell you what happened on a single node. Traces tell you what happened across the
entire request path: Pingora → cold-start → Supervisor → Wasm execution → DB query.

Without tracing, debugging a slow request requires correlating logs from three different
processes by timestamp. With tracing, you get a single timeline showing exactly where
the latency came from.

The OTLP exporter is optional — operators who don't need distributed tracing just omit
`--otlp-endpoint`. The platform works perfectly without it.

### Fuel Metrics: The Unique Value of This Platform

Traditional platforms track CPU percentage. This platform tracks **fuel consumption**.
The difference:

- `cpu_percent = 80%` tells you the CPU is busy, but not which app is responsible or
  whether the cost is justified
- `wasm_fuel_consumed_total{app="api-users"}` tells you exactly how much compute
  `api-users` consumed, in absolute deterministic units

This enables:
1. **Accurate per-tenant billing**: bill by fuel units consumed, not by wall-clock time
2. **Anomaly detection**: a sudden spike in fuel usage without a spike in request count
   means an individual request is doing 100x more computation than normal (bug or attack)
3. **Quota tuning**: when `fuel_quota` is set too low, the `FuelExhaustion` alert fires
   and operators know to increase it before users see errors

---

---

## 1. Raw Metric Sample

Emitted by the executor after each Wasm invocation.

```rust
// crates/metrics/src/lib.rs
pub mod collector;
pub mod exporter;

use serde::{Deserialize, Serialize};

/// A single execution record, produced by the Supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSample {
    pub app_id: String,
    pub instance_id: String,
    pub timestamp_ms: u64,
    pub fuel_consumed: u64,
    pub fuel_limit: u64,
    pub ram_bytes: usize,
    pub wall_clock_ms: u64,
    pub status_code: u16,   // HTTP response code (200, 500, etc.)
    pub is_trap: bool,
    pub trap_reason: Option<String>,
    pub trace_id: Option<String>,
}
```

---

## 2. Metrics Collector (Non-Blocking)

Uses an `mpsc` channel so the hot path never blocks on I/O.

```rust
// crates/metrics/src/collector.rs
use super::ExecutionSample;
use storage::{metrics::MetricBucket, Store};
use tokio::sync::mpsc;
use std::collections::HashMap;
use std::time::{Duration, UNIX_EPOCH, SystemTime};
use tracing::{info, error};

const CHANNEL_CAPACITY: usize = 10_000;

pub struct MetricsCollector {
    tx: mpsc::Sender<ExecutionSample>,
}

impl MetricsCollector {
    /// Create the collector and start the background aggregation task.
    pub fn start(store: Store) -> Self {
        let (tx, rx) = mpsc::channel::<ExecutionSample>(CHANNEL_CAPACITY);
        tokio::spawn(aggregation_loop(rx, store));
        MetricsCollector { tx }
    }

    /// Record an execution sample. Non-blocking (drops if channel is full).
    pub fn record(&self, sample: ExecutionSample) {
        if let Err(_) = self.tx.try_send(sample) {
            tracing::warn!("metrics channel full, dropping sample");
        }
    }

    pub fn sender(&self) -> mpsc::Sender<ExecutionSample> {
        self.tx.clone()
    }
}

/// Background task: accumulates samples and flushes to redb once per minute.
async fn aggregation_loop(mut rx: mpsc::Receiver<ExecutionSample>, store: Store) {
    // In-memory accumulators: app_id → accumulated bucket data
    let mut buckets: HashMap<String, InProgressBucket> = HashMap::new();
    let mut flush_interval = tokio::time::interval(Duration::from_secs(60));

    loop {
        tokio::select! {
            Some(sample) = rx.recv() => {
                let minute_ts = floor_to_minute(sample.timestamp_ms);
                let bucket = buckets.entry(sample.app_id.clone())
                    .or_insert_with(|| InProgressBucket::new(&sample.app_id, minute_ts));
                bucket.add(&sample);
            }
            _ = flush_interval.tick() => {
                let finished: Vec<_> = buckets.drain().collect();
                for (_, bucket) in finished {
                    let mb = bucket.finalize();
                    if let Err(e) = store.write_metric_bucket(&mb) {
                        error!(error = %e, "failed to write metric bucket");
                    }
                }
                // Prune old metrics (keep 7 days)
                store.prune_old_metrics(60 * 24 * 7).ok();
            }
        }
    }
}

struct InProgressBucket {
    app_id: String,
    minute_ts: u64,
    count: u64,
    fuel_sum: u64,
    ram_peak: u64,
    latency_samples: Vec<u64>,
    trap_count: u64,
}

impl InProgressBucket {
    fn new(app_id: &str, minute_ts: u64) -> Self {
        InProgressBucket {
            app_id: app_id.to_string(),
            minute_ts,
            count: 0,
            fuel_sum: 0,
            ram_peak: 0,
            latency_samples: Vec::new(),
            trap_count: 0,
        }
    }

    fn add(&mut self, s: &ExecutionSample) {
        self.count += 1;
        self.fuel_sum += s.fuel_consumed;
        self.ram_peak = self.ram_peak.max(s.ram_bytes as u64);
        self.latency_samples.push(s.wall_clock_ms);
        if s.is_trap { self.trap_count += 1; }
    }

    fn finalize(mut self) -> MetricBucket {
        self.latency_samples.sort_unstable();
        let n = self.latency_samples.len();
        let p50 = percentile(&self.latency_samples, 50) as f64;
        let p99 = percentile(&self.latency_samples, 99) as f64;
        MetricBucket {
            app_id: self.app_id,
            minute_ts: self.minute_ts,
            request_count: self.count,
            fuel_consumed_total: self.fuel_sum,
            fuel_consumed_avg: if self.count > 0 { self.fuel_sum / self.count } else { 0 },
            ram_usage_peak_bytes: self.ram_peak,
            latency_p50_ms: p50,
            latency_p99_ms: p99,
            trap_count: self.trap_count,
        }
    }
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() { return 0; }
    let idx = (sorted.len() * p / 100).min(sorted.len() - 1);
    sorted[idx]
}

fn floor_to_minute(timestamp_ms: u64) -> u64 {
    (timestamp_ms / 1000 / 60) * 60
}
```

---

## 3. Prometheus Exporter

Exposes all metrics in the standard Prometheus text format on `/metrics`.

```rust
// crates/metrics/src/exporter.rs
use prometheus::{
    Counter, CounterVec, Gauge, GaugeVec, Histogram, HistogramOpts, HistogramVec,
    IntCounterVec, Opts, Registry,
};
use axum::{routing::get, Router, response::IntoResponse};
use std::sync::Arc;

#[derive(Clone)]
pub struct Metrics {
    pub registry: Arc<Registry>,
    pub requests_total: IntCounterVec,
    pub fuel_consumed_total: CounterVec,
    pub ram_usage_bytes: GaugeVec,
    pub request_duration_seconds: HistogramVec,
    pub active_instances: GaugeVec,
    pub trap_total: IntCounterVec,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let requests_total = IntCounterVec::new(
            Opts::new("wasm_requests_total", "Total HTTP requests handled"),
            &["app", "status"],
        ).unwrap();
        registry.register(Box::new(requests_total.clone())).unwrap();

        let fuel_consumed_total = CounterVec::new(
            Opts::new("wasm_fuel_consumed_total", "Total fuel units consumed"),
            &["app"],
        ).unwrap();
        registry.register(Box::new(fuel_consumed_total.clone())).unwrap();

        let ram_usage_bytes = GaugeVec::new(
            Opts::new("wasm_ram_usage_bytes", "Current linear memory usage"),
            &["app"],
        ).unwrap();
        registry.register(Box::new(ram_usage_bytes.clone())).unwrap();

        let request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "wasm_request_duration_seconds",
                "Request wall-clock duration in seconds",
            ).buckets(vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5]),
            &["app"],
        ).unwrap();
        registry.register(Box::new(request_duration_seconds.clone())).unwrap();

        let active_instances = GaugeVec::new(
            Opts::new("wasm_active_instances", "Number of running Wasm instances"),
            &["app"],
        ).unwrap();
        registry.register(Box::new(active_instances.clone())).unwrap();

        let trap_total = IntCounterVec::new(
            Opts::new("wasm_trap_total", "Total Wasm trap events (OOM, out-of-fuel)"),
            &["app", "reason"],
        ).unwrap();
        registry.register(Box::new(trap_total.clone())).unwrap();

        Metrics {
            registry: Arc::new(registry),
            requests_total,
            fuel_consumed_total,
            ram_usage_bytes,
            request_duration_seconds,
            active_instances,
            trap_total,
        }
    }

    pub fn record_execution(&self, sample: &super::ExecutionSample) {
        let app = &sample.app_id;
        let status = sample.status_code.to_string();

        self.requests_total.with_label_values(&[app, &status]).inc();
        self.fuel_consumed_total.with_label_values(&[app])
            .inc_by(sample.fuel_consumed as f64);
        self.ram_usage_bytes.with_label_values(&[app])
            .set(sample.ram_bytes as f64);
        self.request_duration_seconds.with_label_values(&[app])
            .observe(sample.wall_clock_ms as f64 / 1000.0);

        if sample.is_trap {
            let reason = sample.trap_reason.as_deref().unwrap_or("unknown");
            self.trap_total.with_label_values(&[app, reason]).inc();
        }
    }
}

/// Axum handler: returns all metrics in Prometheus text format.
pub async fn metrics_handler(
    axum::extract::State(metrics): axum::extract::State<Arc<Metrics>>,
) -> impl IntoResponse {
    use prometheus::Encoder;
    let mut buf = Vec::new();
    let encoder = prometheus::TextEncoder::new();
    encoder.encode(&metrics.registry.gather(), &mut buf).unwrap();
    (
        [("content-type", "text/plain; version=0.0.4")],
        buf,
    )
}

pub fn metrics_router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics)
}
```

---

## 4. OpenTelemetry Tracing

Distributed traces linking Pingora → Supervisor → Wasm execution.

```rust
// crates/metrics/src/tracing_setup.rs
use opentelemetry::global;
use opentelemetry_otlp::WithExportConfig;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize OpenTelemetry with OTLP export (to Jaeger or Grafana Tempo).
pub fn init_tracing(service_name: &str, otlp_endpoint: &str) {
    // 1. Set up OTLP exporter
    let tracer = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(otlp_endpoint),
        )
        .with_trace_config(
            opentelemetry::sdk::trace::config()
                .with_resource(opentelemetry::sdk::Resource::new(vec![
                    opentelemetry::KeyValue::new("service.name", service_name.to_string()),
                ]))
        )
        .install_batch(opentelemetry::runtime::Tokio)
        .expect("OTLP tracer init failed");

    // 2. Combine with tracing subscriber
    let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().json())
        .with(telemetry)
        .init();
}
```

### Propagating trace_id into Wasm

```rust
// In the Supervisor spawn() call, pass the trace ID as an env var
let trace_id = get_current_trace_id(); // extract from current span
env_vars.push(("TRACE_ID".to_string(), trace_id));

// The Wasm app can then include TRACE_ID in its own logs:
// let trace_id = std::env::var("TRACE_ID").unwrap_or_default();
```

---

## 5. Fuel Efficiency Dashboard (Grafana)

Grafana queries to surface on your dashboard:

```promql
# Fuel consumption per app over last 5 minutes
rate(wasm_fuel_consumed_total[5m])

# Average fuel per request
rate(wasm_fuel_consumed_total[5m]) / rate(wasm_requests_total[5m])

# p99 latency by app
histogram_quantile(0.99, rate(wasm_request_duration_seconds_bucket[5m]))

# Trap rate (anomaly detection)
rate(wasm_trap_total[5m])

# Active instances per app
wasm_active_instances
```

---

## 6. Alerting Rules

Define in Prometheus alert rules:

```yaml
# prometheus/alerts.yml
groups:
  - name: wasm_platform
    rules:
      - alert: HighTrapRate
        expr: rate(wasm_trap_total[5m]) > 0.01
        for: 2m
        annotations:
          summary: "App {{ $labels.app }} has a high trap rate"
          description: "More than 1% of requests are triggering Wasm traps"

      - alert: FuelExhaustion
        expr: rate(wasm_trap_total{reason="out_of_fuel"}[5m]) > 0
        for: 1m
        annotations:
          summary: "App {{ $labels.app }} is running out of fuel"
          description: "Increase fuel_quota in the app config"

      - alert: HighLatency
        expr: histogram_quantile(0.99, rate(wasm_request_duration_seconds_bucket[5m])) > 1.0
        for: 5m
        annotations:
          summary: "App {{ $labels.app }} p99 latency > 1s"
```

---

## 7. NATS Control-Plane Monitoring

### The Problem

NATS is the nervous system of the cluster. Every deploy command, health report, scale
event, and rolling upgrade flows through it. If NATS degrades — consumer lag builds up,
messages are dropped, or JetStream disk fills — the platform appears to work (Wasm
instances serve traffic) but cluster-level operations silently break:

- Deploys never reach nodes
- Scale decisions are based on stale data
- Rolling upgrades stall mid-way
- Node failures go undetected

Without NATS-specific metrics, the first symptom is user-visible (a deploy that never
completes, a node silently running stale code) and the root cause is invisible.

### 7.1 NATS Metrics Collector

```rust
// crates/metrics/src/nats_monitor.rs
use nats::jetstream::Context as JetStreamContext;
use prometheus::{GaugeVec, IntCounterVec, Opts, Registry};
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, warn};

/// NATS-specific metrics exposed via the same Prometheus /metrics endpoint.
#[derive(Clone)]
pub struct NatsMetrics {
    /// Number of messages pending in each JetStream consumer (consumer lag).
    /// High lag = this node is falling behind on control-plane events.
    pub consumer_pending: GaugeVec,

    /// Number of messages that were redelivered (ack timeout expired).
    /// Non-zero = a consumer is too slow or crashed mid-processing.
    pub consumer_redelivered: IntCounterVec,

    /// JetStream stream size in bytes on this node.
    /// Monitors disk pressure from accumulated events.
    pub stream_bytes: GaugeVec,

    /// Number of messages in each stream.
    pub stream_messages: GaugeVec,

    /// NATS connection state: 1 = connected, 0 = disconnected.
    pub connection_healthy: GaugeVec,

    /// Total number of NATS reconnection events since startup.
    pub reconnect_count: IntCounterVec,
}

impl NatsMetrics {
    pub fn register(registry: &Registry) -> Self {
        let consumer_pending = GaugeVec::new(
            Opts::new("nats_consumer_pending_messages", "Pending messages in JetStream consumer"),
            &["stream", "consumer"],
        ).unwrap();
        registry.register(Box::new(consumer_pending.clone())).unwrap();

        let consumer_redelivered = IntCounterVec::new(
            Opts::new("nats_consumer_redelivered_total", "Messages redelivered due to ack timeout"),
            &["stream", "consumer"],
        ).unwrap();
        registry.register(Box::new(consumer_redelivered.clone())).unwrap();

        let stream_bytes = GaugeVec::new(
            Opts::new("nats_stream_bytes", "Total bytes stored in JetStream stream"),
            &["stream"],
        ).unwrap();
        registry.register(Box::new(stream_bytes.clone())).unwrap();

        let stream_messages = GaugeVec::new(
            Opts::new("nats_stream_messages", "Total messages in JetStream stream"),
            &["stream"],
        ).unwrap();
        registry.register(Box::new(stream_messages.clone())).unwrap();

        let connection_healthy = GaugeVec::new(
            Opts::new("nats_connection_healthy", "NATS connection state (1=connected, 0=disconnected)"),
            &["node"],
        ).unwrap();
        registry.register(Box::new(connection_healthy.clone())).unwrap();

        let reconnect_count = IntCounterVec::new(
            Opts::new("nats_reconnect_total", "Total NATS reconnection events"),
            &["node"],
        ).unwrap();
        registry.register(Box::new(reconnect_count.clone())).unwrap();

        NatsMetrics {
            consumer_pending,
            consumer_redelivered,
            stream_bytes,
            stream_messages,
            connection_healthy,
            reconnect_count,
        }
    }
}
```

### 7.2 Background Polling Loop

NATS does not push metrics — the node must poll the JetStream API periodically. The
interval is 30 seconds: frequent enough to catch lag buildup, infrequent enough to
avoid adding meaningful load to the NATS server.

```rust
// crates/metrics/src/nats_monitor.rs (continued)

/// Polls JetStream stream and consumer info, updating Prometheus gauges.
/// Runs as a background Tokio task alongside the metrics aggregation loop.
pub async fn nats_monitor_loop(
    js: JetStreamContext,
    metrics: NatsMetrics,
    node_id: String,
    streams: Vec<String>,  // e.g. ["DEPLOY_EVENTS", "HEALTH_REPORTS", "SCALE_EVENTS"]
) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));

    loop {
        interval.tick().await;

        // Update connection health
        // The NATS client reconnects automatically; we track whether it's currently connected.
        metrics.connection_healthy
            .with_label_values(&[&node_id])
            .set(1.0); // If this task is running, the connection is alive.

        for stream_name in &streams {
            match js.stream_info(stream_name).await {
                Ok(info) => {
                    metrics.stream_bytes
                        .with_label_values(&[stream_name])
                        .set(info.state.bytes as f64);
                    metrics.stream_messages
                        .with_label_values(&[stream_name])
                        .set(info.state.messages as f64);

                    // Poll each consumer on this stream
                    if let Ok(consumers) = js.consumer_names(stream_name).await {
                        for consumer_name in consumers {
                            match js.consumer_info(stream_name, &consumer_name).await {
                                Ok(ci) => {
                                    metrics.consumer_pending
                                        .with_label_values(&[stream_name, &consumer_name])
                                        .set(ci.num_pending as f64);
                                    // num_redelivered is cumulative — use as counter
                                    metrics.consumer_redelivered
                                        .with_label_values(&[stream_name, &consumer_name])
                                        .inc_by(ci.num_redelivered as u64);
                                }
                                Err(e) => {
                                    warn!(
                                        stream = stream_name,
                                        consumer = consumer_name,
                                        error = %e,
                                        "failed to fetch consumer info"
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(
                        stream = stream_name,
                        error = %e,
                        "failed to fetch stream info"
                    );
                    // If we can't reach JetStream, mark connection as unhealthy
                    metrics.connection_healthy
                        .with_label_values(&[&node_id])
                        .set(0.0);
                }
            }
        }
    }
}
```

### 7.3 Integration with Node Startup

```rust
// crates/node/src/main.rs (updated excerpt)
use metrics::nats_monitor::{NatsMetrics, nats_monitor_loop};

// During node initialization, after NATS connection is established:
let nats_metrics = NatsMetrics::register(&prom_metrics.registry);
tokio::spawn(nats_monitor_loop(
    jetstream_context.clone(),
    nats_metrics,
    node_id.clone(),
    vec![
        "DEPLOY_EVENTS".to_string(),
        "HEALTH_REPORTS".to_string(),
        "SCALE_EVENTS".to_string(),
    ],
));
```

### 7.4 NATS Alerting Rules

```yaml
# prometheus/alerts.yml (additions)
      - alert: NatsConsumerLag
        expr: nats_consumer_pending_messages > 1000
        for: 2m
        annotations:
          summary: "Consumer {{ $labels.consumer }} on {{ $labels.stream }} has >1000 pending"
          description: "Node is falling behind on control-plane events. Check node health and NATS throughput."

      - alert: NatsDisconnected
        expr: nats_connection_healthy == 0
        for: 30s
        annotations:
          summary: "Node {{ $labels.node }} lost NATS connection"
          description: "Cluster operations (deploy, scale, health) are paused on this node."

      - alert: NatsStreamDiskPressure
        expr: nats_stream_bytes > 1073741824  # 1 GB
        for: 5m
        annotations:
          summary: "JetStream stream {{ $labels.stream }} exceeds 1 GB"
          description: "Check retention policy and consumer lag. Old messages may not be acking."

      - alert: NatsHighRedelivery
        expr: rate(nats_consumer_redelivered_total[5m]) > 10
        for: 2m
        annotations:
          summary: "Consumer {{ $labels.consumer }} redelivering >10 msg/s"
          description: "Messages are timing out before being processed. Consumer may be stuck."
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Execution Sample Collection
- [ ] `MetricsCollector::start()` launches a background aggregation task without blocking
- [ ] `collector.record(sample)` is non-blocking — it never waits for a disk write
- [ ] If the channel is full, `record()` drops the sample and logs a warning — it does not panic
- [ ] After 60 seconds of inactivity, buffered samples are flushed to redb

### MetricBucket Aggregation
- [ ] A batch of 1000 samples for the same app is aggregated into a single `MetricBucket` per minute
- [ ] `fuel_consumed_avg` equals `fuel_consumed_total / request_count`
- [ ] `ram_usage_peak_bytes` is the maximum ram across all samples in the bucket (not the average)
- [ ] `latency_p50_ms` and `latency_p99_ms` are correctly computed percentiles

### redb Storage
- [ ] `write_metric_bucket()` stores the bucket without error
- [ ] `load_recent_metrics(app_id, 60)` returns only the last 60 minutes of buckets for that app
- [ ] `prune_old_metrics(10080)` (7 days) deletes buckets older than 7 days and returns the count

### Prometheus Endpoint
- [ ] `GET /metrics` returns HTTP 200 with `Content-Type: text/plain; version=0.0.4`
- [ ] The response contains `wasm_requests_total`, `wasm_fuel_consumed_total`, `wasm_request_duration_seconds`
- [ ] `wasm_active_instances` gauge updates correctly when instances are spawned and killed
- [ ] `wasm_trap_total` increments when a Trap occurs
- [ ] Prometheus can successfully scrape the `/metrics` endpoint (verified with `curl` or `promtool`)

### OpenTelemetry
- [ ] `init_tracing()` initializes without error when an OTLP endpoint is provided
- [ ] Spans are created for each Wasm execution and include `app_id` and `instance_id` attributes
- [ ] `trace_id` is injected into the Wasm env var and appears in structured logs

### Tests
- [ ] A test sends 100 samples to the collector and verifies a correct bucket is written to redb after 60s (or forced flush)
- [ ] A test verifies that the Prometheus endpoint returns valid text format

### NATS Monitoring
- [ ] `NatsMetrics::register()` registers all 6 metric families in the Prometheus registry
- [ ] `nats_monitor_loop` polls JetStream stream info every 30 seconds
- [ ] `nats_consumer_pending_messages` reflects actual consumer lag for each stream/consumer pair
- [ ] `nats_consumer_redelivered_total` increments when messages are redelivered
- [ ] `nats_stream_bytes` and `nats_stream_messages` reflect current stream state
- [ ] `nats_connection_healthy` flips to 0 when JetStream API calls fail, back to 1 on recovery
- [ ] `nats_reconnect_total` increments on each NATS reconnection event
- [ ] All NATS metrics appear on `GET /metrics` alongside the existing Wasm execution metrics
- [ ] `NatsConsumerLag`, `NatsDisconnected`, `NatsStreamDiskPressure`, and `NatsHighRedelivery` alerts are defined in Prometheus rules

// crates/metrics/src/exporter.rs
use axum::{response::IntoResponse, routing::get, Router};
use prometheus::{
    CounterVec, GaugeVec, HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry,
};
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
    pub platform_info: IntCounterVec,
}

impl Metrics {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let registry = Registry::new();

        let requests_total = IntCounterVec::new(
            Opts::new("wasm_requests_total", "Total HTTP requests handled"),
            &["app", "status"],
        )
        .unwrap();
        registry.register(Box::new(requests_total.clone())).unwrap();

        let fuel_consumed_total = CounterVec::new(
            Opts::new("wasm_fuel_consumed_total", "Total fuel units consumed"),
            &["app"],
        )
        .unwrap();
        registry
            .register(Box::new(fuel_consumed_total.clone()))
            .unwrap();

        let ram_usage_bytes = GaugeVec::new(
            Opts::new("wasm_ram_usage_bytes", "Current linear memory usage"),
            &["app"],
        )
        .unwrap();
        registry
            .register(Box::new(ram_usage_bytes.clone()))
            .unwrap();

        let request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "wasm_request_duration_seconds",
                "Request wall-clock duration in seconds",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
            ]),
            &["app"],
        )
        .unwrap();
        registry
            .register(Box::new(request_duration_seconds.clone()))
            .unwrap();

        let active_instances = GaugeVec::new(
            Opts::new("wasm_active_instances", "Number of running Wasm instances"),
            &["app"],
        )
        .unwrap();
        registry
            .register(Box::new(active_instances.clone()))
            .unwrap();

        let trap_total = IntCounterVec::new(
            Opts::new(
                "wasm_trap_total",
                "Total Wasm trap events (OOM, out-of-fuel)",
            ),
            &["app", "reason"],
        )
        .unwrap();
        registry.register(Box::new(trap_total.clone())).unwrap();

        let platform_info = IntCounterVec::new(
            Opts::new(
                "wasm_platform_info",
                "Platform version information (value is always 1)",
            ),
            &["node_id", "binary_version", "protocol_version"],
        )
        .unwrap();
        registry.register(Box::new(platform_info.clone())).unwrap();

        Metrics {
            registry: Arc::new(registry),
            requests_total,
            fuel_consumed_total,
            ram_usage_bytes,
            request_duration_seconds,
            active_instances,
            trap_total,
            platform_info,
        }
    }

    /// Set the platform version info metric. This should be called once at startup.
    pub fn set_platform_info(&self, node_id: &str, binary_version: &str, protocol_version: u32) {
        self.platform_info
            .with_label_values(&[node_id, binary_version, &protocol_version.to_string()])
            .inc();
    }

    pub fn record_execution(&self, sample: &super::ExecutionSample) {
        let app = &sample.app_id;
        let status = sample.status_code.to_string();

        self.requests_total.with_label_values(&[app, &status]).inc();
        self.fuel_consumed_total
            .with_label_values(&[app])
            .inc_by(sample.fuel_consumed as f64);
        self.ram_usage_bytes
            .with_label_values(&[app])
            .set(sample.ram_bytes as f64);
        self.request_duration_seconds
            .with_label_values(&[app])
            .observe(sample.wall_clock_ms as f64 / 1000.0);

        if sample.is_trap {
            let reason = sample.trap_reason.as_deref().unwrap_or("unknown");
            let app_owned = app.to_string();
            let reason_owned = reason.to_string();
            self.trap_total.with_label_values(&[&app_owned, &reason_owned]).inc();
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
    encoder
        .encode(&metrics.registry.gather(), &mut buf)
        .unwrap();
    ([("content-type", "text/plain; version=0.0.4")], buf)
}

pub fn metrics_router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics)
}

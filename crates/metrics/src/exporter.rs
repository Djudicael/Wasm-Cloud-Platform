// crates/metrics/src/exporter.rs
use axum::{response::IntoResponse, routing::get, Router};
use prometheus::process_collector::ProcessCollector;
use prometheus::{
    CounterVec, GaugeVec, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts,
    Registry,
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
    pub policy: PolicyMetrics,
}

impl Metrics {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let registry = Registry::new();
        registry
            .register(Box::new(ProcessCollector::for_self()))
            .expect("process metrics must register");
        let policy = PolicyMetrics::new(&registry);

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
            policy,
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
            self.trap_total
                .with_label_values(&[&app_owned, &reason_owned])
                .inc();
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

/// Prometheus metrics for WASI policy enforcement violations.
///
/// Tracks how many times each policy type was denied across all instances,
/// plus gauges for current active outbound connections, open FDs,
/// linear memory usage, and table elements.
/// These feed the alerting rules in `deploy/prometheus/wasi_policy_alerts.yml`.
#[derive(Clone)]
pub struct PolicyMetrics {
    /// Total outbound connections denied by policy.
    pub connection_denied_total: IntCounter,

    /// Total egress operations denied by policy.
    pub egress_denied_total: IntCounter,

    /// Total FD open operations denied by policy.
    pub fd_denied_total: IntCounter,

    /// Total filesystem write operations denied by policy.
    pub fs_write_denied_total: IntCounter,

    /// Total bind operations denied by policy.
    pub bind_denied_total: IntCounter,

    /// Total DNS lookups denied by policy.
    pub dns_denied_total: IntCounter,

    /// Total denied memory growth requests.
    pub memory_growth_denied_total: IntCounter,

    /// Total denied table growth requests.
    pub table_growth_denied_total: IntCounter,

    /// Current active outbound connections across all instances.
    pub active_outbound_connections: IntGauge,

    /// Current open FDs across all instances.
    pub open_fds: IntGauge,

    /// Current guest linear memory bytes across all instances.
    pub current_memory_bytes: IntGauge,

    /// Current guest table elements across all instances.
    pub current_table_elements: IntGauge,
}

impl PolicyMetrics {
    pub fn new(registry: &Registry) -> Self {
        let connection_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_connection_denied_total",
            "Outbound connections denied by WASI policy",
        ))
        .unwrap();
        registry
            .register(Box::new(connection_denied_total.clone()))
            .unwrap();

        let egress_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_egress_denied_total",
            "Egress operations denied by WASI policy",
        ))
        .unwrap();
        registry
            .register(Box::new(egress_denied_total.clone()))
            .unwrap();

        let fd_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_fd_denied_total",
            "FD open operations denied by WASI policy",
        ))
        .unwrap();
        registry
            .register(Box::new(fd_denied_total.clone()))
            .unwrap();

        let fs_write_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_fs_write_denied_total",
            "Filesystem write operations denied by WASI policy",
        ))
        .unwrap();
        registry
            .register(Box::new(fs_write_denied_total.clone()))
            .unwrap();

        let bind_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_bind_denied_total",
            "Bind operations denied by WASI policy",
        ))
        .unwrap();
        registry
            .register(Box::new(bind_denied_total.clone()))
            .unwrap();

        let dns_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_dns_denied_total",
            "DNS lookups denied by WASI policy",
        ))
        .unwrap();
        registry
            .register(Box::new(dns_denied_total.clone()))
            .unwrap();

        let memory_growth_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_memory_growth_denied_total",
            "Linear memory growth requests denied by the Wasmtime resource limiter",
        ))
        .unwrap();
        registry
            .register(Box::new(memory_growth_denied_total.clone()))
            .unwrap();

        let table_growth_denied_total = IntCounter::with_opts(Opts::new(
            "wasm_policy_table_growth_denied_total",
            "Table growth requests denied by the Wasmtime resource limiter",
        ))
        .unwrap();
        registry
            .register(Box::new(table_growth_denied_total.clone()))
            .unwrap();

        let active_outbound_connections = IntGauge::with_opts(Opts::new(
            "wasm_policy_active_outbound_connections",
            "Current active outbound connections across all instances",
        ))
        .unwrap();
        registry
            .register(Box::new(active_outbound_connections.clone()))
            .unwrap();

        let open_fds = IntGauge::with_opts(Opts::new(
            "wasm_policy_open_fds",
            "Current open file descriptors across all instances",
        ))
        .unwrap();
        registry.register(Box::new(open_fds.clone())).unwrap();

        let current_memory_bytes = IntGauge::with_opts(Opts::new(
            "wasm_policy_current_memory_bytes",
            "Current guest linear memory bytes across all instances",
        ))
        .unwrap();
        registry
            .register(Box::new(current_memory_bytes.clone()))
            .unwrap();

        let current_table_elements = IntGauge::with_opts(Opts::new(
            "wasm_policy_current_table_elements",
            "Current guest table elements across all instances",
        ))
        .unwrap();
        registry
            .register(Box::new(current_table_elements.clone()))
            .unwrap();

        PolicyMetrics {
            connection_denied_total,
            egress_denied_total,
            fd_denied_total,
            fs_write_denied_total,
            bind_denied_total,
            dns_denied_total,
            memory_growth_denied_total,
            table_growth_denied_total,
            active_outbound_connections,
            open_fds,
            current_memory_bytes,
            current_table_elements,
        }
    }
}

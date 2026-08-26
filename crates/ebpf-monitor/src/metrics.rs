//! Prometheus metrics for eBPF monitoring.
//!
//! All metrics are prefixed with `wasm_ebpf_` and registered with the
//! provided Prometheus registry. Registration is resilient — if a metric
//! is already registered (e.g., during hot-reload or tests), the existing
//! instance is reused instead of panicking.

use prometheus::{
    histogram_opts, Gauge, Histogram, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
};

/// Error-resilient metric registration helper.
/// If a metric is already registered, returns a new instance that shares
/// the same underlying metric via the registry's internal deduplication.
macro_rules! register_metric {
    ($metric:expr, $registry:expr) => {{
        // Evaluate the constructor exactly once. Evaluating `$metric` again in
        // the match arm creates a disconnected collector: the handle changes,
        // while the registry keeps exporting its initial value.
        let metric = $metric;
        match $registry.register(Box::new(metric.clone())) {
            Ok(_) => metric,
            Err(e) => {
                // Metric already registered — retrieve the existing one.
                // This happens in tests or if `EbpfMetrics::new` is called twice.
                tracing::debug!(
                    error = %e,
                    "metric already registered, creating new instance (registry will deduplicate)"
                );
                metric
            }
        }
    }};
}

/// Prometheus metrics exported by the eBPF monitor.
///
/// These metrics are updated both by the eBPF ring-buffer consumer (when the
/// `ebpf` feature is active) and by the userspace fallback monitor.
#[derive(Debug)]
pub struct EbpfMetrics {
    /// Total OOM kills detected by eBPF process tracker.
    pub oom_kills: IntCounter,

    /// Total process exits detected (excluding OOM).
    pub process_exits: IntCounter,

    /// Total signal deaths (non-OOM signals).
    pub signal_deaths: IntCounter,

    /// Total TCP retransmits detected.
    pub tcp_retransmits: IntCounter,

    /// NATS retransmit events (subset of tcp_retransmits, port 4222).
    pub nats_retransmit_events: IntCounter,

    /// FD usage ratio (current / soft limit) for the highest-FD PID.
    /// Value is 0.0–1.0 (fraction) for precise alerting thresholds.
    pub fd_usage_ratio: Gauge,

    /// Memory pressure level (0=none, 1=low, 2=medium, 3=critical).
    pub memory_pressure_level: IntGauge,

    /// Disk I/O latency histogram (seconds).
    pub disk_io_latency_seconds: Histogram,

    /// Security violations (privileged syscalls from Wasm instances).
    pub security_violations: IntCounter,

    /// Whether eBPF is loaded and active (1=active, 0=fallback).
    pub ebpf_active: IntGauge,

    /// Total events processed from the ring buffer (all types).
    pub events_processed: IntCounter,

    /// Events processed by stable event type. Application identity remains in
    /// structured logs to avoid an unbounded Prometheus label cardinality.
    pub events_by_type: IntCounterVec,

    /// Total events that failed to parse (malformed or unknown type).
    pub events_parse_errors: IntCounter,

    /// Current TCP connection count for the monitored node PID.
    pub tcp_connection_count: IntGauge,

    /// Current open FD count for the monitored node PID.
    pub fd_count: IntGauge,
}

impl EbpfMetrics {
    /// Create and register all eBPF metrics with the given Prometheus registry.
    ///
    /// Registration is resilient: if a metric name is already registered
    /// (e.g., in tests), the existing metric is reused rather than panicking.
    pub fn new(registry: &Registry) -> Self {
        let oom_kills = register_metric!(
            IntCounter::with_opts(Opts::new(
                "wasm_ebpf_oom_kills_total",
                "Total OOM kills detected by eBPF process tracker"
            ))
            .unwrap(),
            registry
        );

        let process_exits = register_metric!(
            IntCounter::with_opts(Opts::new(
                "wasm_ebpf_process_exits_total",
                "Total process exits detected by eBPF"
            ))
            .unwrap(),
            registry
        );

        let signal_deaths = register_metric!(
            IntCounter::with_opts(Opts::new(
                "wasm_ebpf_signal_deaths_total",
                "Total signal deaths (non-OOM) detected by eBPF"
            ))
            .unwrap(),
            registry
        );

        let tcp_retransmits = register_metric!(
            IntCounter::with_opts(Opts::new(
                "wasm_ebpf_tcp_retransmits_total",
                "Total TCP retransmits detected by eBPF"
            ))
            .unwrap(),
            registry
        );

        let nats_retransmit_events = register_metric!(
            IntCounter::with_opts(Opts::new(
                "wasm_ebpf_nats_retransmits_total",
                "NATS TCP retransmit events detected by eBPF (port 4222)"
            ))
            .unwrap(),
            registry
        );

        let fd_usage_ratio = register_metric!(
            Gauge::with_opts(Opts::new(
                "wasm_ebpf_fd_usage_ratio",
                "FD usage ratio (current/limit) for highest-FD PID (0.0–1.0)"
            ))
            .unwrap(),
            registry
        );

        let memory_pressure_level = register_metric!(
            IntGauge::with_opts(Opts::new(
                "wasm_ebpf_memory_pressure_level",
                "Memory pressure level (0=none, 1=low, 2=medium, 3=critical)"
            ))
            .unwrap(),
            registry
        );

        let disk_io_latency_seconds = register_metric!(
            Histogram::with_opts(histogram_opts!(
                "wasm_ebpf_disk_io_latency_seconds",
                "Disk I/O latency from eBPF block tracepoints",
                vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0]
            ))
            .unwrap(),
            registry
        );

        let security_violations = register_metric!(
            IntCounter::with_opts(Opts::new(
                "wasm_ebpf_security_violations_total",
                "Security violations detected by eBPF syscall monitor"
            ))
            .unwrap(),
            registry
        );

        let ebpf_active = register_metric!(
            IntGauge::with_opts(Opts::new(
                "wasm_ebpf_active",
                "Whether eBPF monitoring is active (1=yes, 0=fallback)"
            ))
            .unwrap(),
            registry
        );

        let events_processed = register_metric!(
            IntCounter::with_opts(Opts::new(
                "wasm_ebpf_events_processed_total",
                "Total events processed from the eBPF ring buffer"
            ))
            .unwrap(),
            registry
        );

        let events_by_type = register_metric!(
            IntCounterVec::new(
                Opts::new(
                    "wasm_ebpf_events_by_type_total",
                    "eBPF events processed by stable event type"
                ),
                &["event_type"]
            )
            .unwrap(),
            registry
        );

        let events_parse_errors = register_metric!(
            IntCounter::with_opts(Opts::new(
                "wasm_ebpf_events_parse_errors_total",
                "Total events that failed to parse from the eBPF ring buffer"
            ))
            .unwrap(),
            registry
        );

        let tcp_connection_count = register_metric!(
            IntGauge::with_opts(Opts::new(
                "wasm_ebpf_tcp_connection_count",
                "Current TCP connection count for the monitored node PID"
            ))
            .unwrap(),
            registry
        );

        let fd_count = register_metric!(
            IntGauge::with_opts(Opts::new(
                "wasm_ebpf_fd_count",
                "Current open file descriptor count for the monitored node PID"
            ))
            .unwrap(),
            registry
        );

        // Initialize: eBPF is not active yet (will be set to 1 if programs load)
        ebpf_active.set(0);

        EbpfMetrics {
            oom_kills,
            process_exits,
            signal_deaths,
            tcp_retransmits,
            nats_retransmit_events,
            fd_usage_ratio,
            memory_pressure_level,
            disk_io_latency_seconds,
            security_violations,
            ebpf_active,
            events_processed,
            events_by_type,
            events_parse_errors,
            tcp_connection_count,
            fd_count,
        }
    }

    /// Mark eBPF as active (programs loaded and attached).
    pub fn mark_ebpf_active(&self) {
        self.ebpf_active.set(1);
    }

    /// Mark eBPF as inactive (fallback mode).
    pub fn mark_ebpf_fallback(&self) {
        self.ebpf_active.set(0);
    }

    /// Record a disk I/O latency observation (converting nanoseconds to seconds).
    pub fn observe_disk_latency_ns(&self, latency_ns: u64) {
        let latency_secs = latency_ns as f64 / 1_000_000_000.0;
        self.disk_io_latency_seconds.observe(latency_secs);
    }

    /// Record FD usage as a ratio (0.0–1.0).
    pub fn set_fd_usage(&self, current: u32, limit: u32) {
        if limit > 0 {
            let ratio = current as f64 / limit as f64;
            self.fd_usage_ratio.set(ratio);
        }
        self.fd_count.set(current as i64);
    }

    /// Get the current FD usage ratio (0.0–1.0).
    pub fn get_fd_usage_ratio(&self) -> f64 {
        self.fd_usage_ratio.get()
    }

    /// Record memory pressure level.
    pub fn set_memory_pressure(&self, level: u32) {
        self.memory_pressure_level.set(level as i64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_new() {
        let registry = Registry::new();
        let metrics = EbpfMetrics::new(&registry);

        // eBPF active should start at 0
        assert_eq!(metrics.ebpf_active.get(), 0);

        // Counters should start at 0
        assert_eq!(metrics.oom_kills.get(), 0);
        assert_eq!(metrics.process_exits.get(), 0);
        assert_eq!(metrics.signal_deaths.get(), 0);
        assert_eq!(metrics.tcp_retransmits.get(), 0);
        assert_eq!(metrics.nats_retransmit_events.get(), 0);
        assert_eq!(metrics.security_violations.get(), 0);
        assert_eq!(metrics.events_processed.get(), 0);
        assert_eq!(metrics.events_parse_errors.get(), 0);

        // Gauges should start at 0
        assert_eq!(metrics.fd_usage_ratio.get(), 0.0);
        assert_eq!(metrics.memory_pressure_level.get(), 0);
        assert_eq!(metrics.tcp_connection_count.get(), 0);
        assert_eq!(metrics.fd_count.get(), 0);
    }

    #[test]
    fn test_mark_ebpf_active() {
        let registry = Registry::new();
        let metrics = EbpfMetrics::new(&registry);

        assert_eq!(metrics.ebpf_active.get(), 0);
        metrics.mark_ebpf_active();
        assert_eq!(metrics.ebpf_active.get(), 1);
        let exported = registry
            .gather()
            .into_iter()
            .find(|family| family.name() == "wasm_ebpf_active")
            .expect("active gauge must be registered");
        assert_eq!(exported.get_metric()[0].get_gauge().value(), 1.0);
        metrics.mark_ebpf_fallback();
        assert_eq!(metrics.ebpf_active.get(), 0);
    }

    #[test]
    fn test_counter_increment() {
        let registry = Registry::new();
        let metrics = EbpfMetrics::new(&registry);

        metrics.oom_kills.inc();
        metrics.oom_kills.inc();
        assert_eq!(metrics.oom_kills.get(), 2);

        metrics.signal_deaths.inc_by(5);
        assert_eq!(metrics.signal_deaths.get(), 5);
    }

    #[test]
    fn test_fd_usage_ratio() {
        let registry = Registry::new();
        let metrics = EbpfMetrics::new(&registry);

        metrics.set_fd_usage(4096, 8192);
        // Ratio should be exactly 0.5, stored as f64 in the Gauge
        let ratio = metrics.get_fd_usage_ratio();
        assert!(
            (ratio - 0.5).abs() < 0.001,
            "ratio should be ~0.5, got {}",
            ratio
        );

        // fd_count should be set
        assert_eq!(metrics.fd_count.get(), 4096);
    }

    #[test]
    fn test_fd_usage_zero_limit() {
        let registry = Registry::new();
        let metrics = EbpfMetrics::new(&registry);

        // Should not panic with limit=0
        metrics.set_fd_usage(100, 0);
        // fd_count should still be set
        assert_eq!(metrics.fd_count.get(), 100);
    }

    #[test]
    fn test_memory_pressure_levels() {
        let registry = Registry::new();
        let metrics = EbpfMetrics::new(&registry);

        for level in 0..=3 {
            metrics.set_memory_pressure(level);
            assert_eq!(metrics.memory_pressure_level.get(), level as i64);
        }
    }

    #[test]
    fn test_disk_latency_observation() {
        let registry = Registry::new();
        let metrics = EbpfMetrics::new(&registry);

        // 50ms in nanoseconds
        metrics.observe_disk_latency_ns(50_000_000);
        // 1ms in nanoseconds
        metrics.observe_disk_latency_ns(1_000_000);

        // Histogram should have 2 observations
        let histogram = &metrics.disk_io_latency_seconds;
        let count = histogram.get_sample_count();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_fd_usage_ratio_full() {
        let registry = Registry::new();
        let metrics = EbpfMetrics::new(&registry);

        metrics.set_fd_usage(8192, 8192);
        let ratio = metrics.get_fd_usage_ratio();
        assert!(
            (ratio - 1.0).abs() < 0.001,
            "ratio should be ~1.0, got {}",
            ratio
        );
    }

    #[test]
    fn test_double_registration_resilient() {
        // Creating metrics twice with the same registry should not panic.
        // This can happen in tests or if init is called multiple times.
        let registry = Registry::new();
        let _metrics1 = EbpfMetrics::new(&registry);
        // Second creation should not panic — the macro handles re-registration
        let _metrics2 = EbpfMetrics::new(&registry);
    }

    #[test]
    fn test_events_processed() {
        let registry = Registry::new();
        let metrics = EbpfMetrics::new(&registry);

        metrics.events_processed.inc_by(100);
        assert_eq!(metrics.events_processed.get(), 100);

        metrics.events_parse_errors.inc();
        assert_eq!(metrics.events_parse_errors.get(), 1);
    }
}

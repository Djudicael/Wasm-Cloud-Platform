//! Prometheus metrics for eBPF monitoring.

use prometheus::{histogram_opts, Histogram, IntCounter, IntGauge, Opts, Registry};

pub struct EbpfMetrics {
    /// Total OOM kills detected by eBPF.
    pub oom_kills: IntCounter,

    /// Total process exits detected (excluding OOM).
    pub process_exits: IntCounter,

    /// Total signal deaths (non-OOM signals).
    pub signal_deaths: IntCounter,

    /// Total TCP retransmits detected.
    pub tcp_retransmits: IntCounter,

    /// NATS retransmit events (subset of tcp_retransmits).
    pub nats_retransmit_events: IntCounter,

    /// FD usage ratio (current / soft limit) for the highest-FD PID.
    pub fd_usage_ratio: IntGauge,

    /// Memory pressure level (0=none, 1=low, 2=medium, 3=critical).
    pub memory_pressure_level: IntGauge,

    /// Disk I/O latency histogram.
    pub disk_io_latency_seconds: Histogram,

    /// Security violations (privileged syscalls from Wasm instances).
    pub security_violations: IntCounter,

    /// Whether eBPF is loaded and active (1=active, 0=fallback).
    pub ebpf_active: IntGauge,
}

impl EbpfMetrics {
    pub fn new(registry: &Registry) -> Self {
        let oom_kills = IntCounter::with_opts(Opts::new(
            "wasm_ebpf_oom_kills_total",
            "Total OOM kills detected by eBPF process tracker",
        ))
        .unwrap();
        registry.register(Box::new(oom_kills.clone())).unwrap();

        let process_exits = IntCounter::with_opts(Opts::new(
            "wasm_ebpf_process_exits_total",
            "Total process exits detected by eBPF",
        ))
        .unwrap();
        registry.register(Box::new(process_exits.clone())).unwrap();

        let signal_deaths = IntCounter::with_opts(Opts::new(
            "wasm_ebpf_signal_deaths_total",
            "Total signal deaths (non-OOM) detected by eBPF",
        ))
        .unwrap();
        registry.register(Box::new(signal_deaths.clone())).unwrap();

        let tcp_retransmits = IntCounter::with_opts(Opts::new(
            "wasm_ebpf_tcp_retransmits_total",
            "Total TCP retransmits detected by eBPF",
        ))
        .unwrap();
        registry
            .register(Box::new(tcp_retransmits.clone()))
            .unwrap();

        let nats_retransmit_events = IntCounter::with_opts(Opts::new(
            "wasm_ebpf_nats_retransmits_total",
            "NATS TCP retransmit events detected by eBPF",
        ))
        .unwrap();
        registry
            .register(Box::new(nats_retransmit_events.clone()))
            .unwrap();

        let fd_usage_ratio = IntGauge::with_opts(Opts::new(
            "wasm_ebpf_fd_usage_ratio",
            "FD usage ratio (current/limit) for highest-FD PID",
        ))
        .unwrap();
        registry.register(Box::new(fd_usage_ratio.clone())).unwrap();

        let memory_pressure_level = IntGauge::with_opts(Opts::new(
            "wasm_ebpf_memory_pressure_level",
            "Memory pressure level (0=none, 1=low, 2=medium, 3=critical)",
        ))
        .unwrap();
        registry
            .register(Box::new(memory_pressure_level.clone()))
            .unwrap();

        let disk_io_latency_seconds = Histogram::with_opts(histogram_opts!(
            "wasm_ebpf_disk_io_latency_seconds",
            "Disk I/O latency from eBPF block tracepoints",
            vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0]
        ))
        .unwrap();
        registry
            .register(Box::new(disk_io_latency_seconds.clone()))
            .unwrap();

        let security_violations = IntCounter::with_opts(Opts::new(
            "wasm_ebpf_security_violations_total",
            "Security violations detected by eBPF syscall monitor",
        ))
        .unwrap();
        registry
            .register(Box::new(security_violations.clone()))
            .unwrap();

        let ebpf_active = IntGauge::with_opts(Opts::new(
            "wasm_ebpf_active",
            "Whether eBPF monitoring is active (1=yes, 0=fallback)",
        ))
        .unwrap();
        registry.register(Box::new(ebpf_active.clone())).unwrap();

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
        }
    }
}

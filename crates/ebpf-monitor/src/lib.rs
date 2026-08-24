//! eBPF Kernel-Level Monitoring & Automated Recovery
//!
//! This crate provides kernel-level observability for the Wasm Cloud Platform
//! using eBPF (when available on Linux >= 5.8 with BTF) or userspace fallback
//! monitoring (on any platform).
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────┐
//! │    eBPF Programs     │  (kernel space, requires nightly + bpfel target)
//! │  process_tracker     │
//! │  tcp_monitor         │
//! │  fd_watcher          │
//! │  mem_pressure        │
//! │  disk_monitor        │
//! │  syscall_counter     │
//! └──────────┬───────────┘
//!            │ ring buffer
//!            ▼
//! ┌──────────────────────┐     mpsc channel    ┌──────────────────┐
//! │   Ring Buffer Consumer │ ────────────────▶ │ Action Dispatcher │
//! │   (parse events)      │   MonitorEvent    │ (metrics + actions)│
//! └──────────────────────┘                    └──────────────────┘
//!                                                 │
//!                                                 ▼
//! ┌──────────────────────────────────────────────────────────────┐
//! │                    EventCallbacks trait                       │
//! │  (implemented by node main.rs with concrete platform types)  │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Feature Flags
//!
//! - `ebpf`: Enable eBPF program loading and ring buffer consumption.
//!   Requires Linux kernel >= 5.8 with BTF support and `CAP_BPF`/`CAP_SYS_ADMIN`.
//!   Without this feature, the crate compiles on any platform but only provides
//!   userspace fallback monitoring (5-second polling interval vs 10ms eBPF).
//!
//! # Usage
//!
//! ```rust,ignore
//! use ebpf_monitor::{MonitorConfig, EbpfMetrics, ActionDispatcher, EventCallbacks, NoopCallbacks};
//! use std::sync::Arc;
//! use prometheus::Registry;
//!
//! let registry = Registry::new();
//! let metrics = Arc::new(EbpfMetrics::new(&registry));
//! let callbacks = Arc::new(NoopCallbacks);
//! let dispatcher = Arc::new(ActionDispatcher::new(
//!     metrics.clone(),
//!     callbacks,
//!     "node-0".to_string(),
//! ));
//! let config = MonitorConfig::default();
//!
//! // In an async context:
//! let handle = ebpf_monitor::init(config, metrics, dispatcher, std::process::id()).await;
//! if handle.is_ebpf_active() {
//!     tracing::info!("eBPF kernel-level monitoring active");
//! } else {
//!     tracing::info!("Userspace fallback monitoring active");
//! }
//! ```

pub mod actions;
pub mod config;
pub mod consumer;
pub mod fallback;
pub mod metrics;
pub mod namespace_map;

pub use namespace_map::{CallerIdentity, NamespaceMap};

// Shared data structures between eBPF programs and userspace.
// Always available — the type definitions don't depend on aya.
// The `Pod` impls for aya map operations are conditionally compiled
// within the module when the `ebpf` feature is enabled.
pub mod common;

#[cfg(feature = "ebpf")]
pub mod loader;

// ── Public Re-exports ──────────────────────────────────────────────────────────

pub use actions::{ActionDispatcher, EventCallbacks, MonitorEvent, NoopCallbacks, RecoveryAction};
pub use config::MonitorConfig;
pub use consumer::{parse_event, start_action_dispatcher, ConsumerConfig, ParseError};
pub use metrics::EbpfMetrics;

#[cfg(feature = "ebpf")]
pub use loader::LoadedEbpf;

// ── Monitor Handle ────────────────────────────────────────────────────────────

use std::sync::Arc;
#[cfg(any(feature = "ebpf", test))]
use std::time::Duration;

use tracing::{info, warn};

/// Snapshot of the eBPF monitor's current state, suitable for
/// serialization in admin API responses.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MonitorStatus {
    /// Whether eBPF programs are loaded and active (vs userspace fallback).
    pub ebpf_active: bool,
    /// Whether backpressure is currently active (rejecting new connections).
    pub backpressure_active: bool,
    /// Whether the node is in degraded mode (slow I/O recovery).
    pub degraded_mode: bool,
    /// Last memory pressure level (0=none, 1=low, 2=medium, 3=critical).
    pub pressure_level: u32,
    /// Total OOM kills detected.
    pub oom_kills: u64,
    /// Total process exits detected.
    pub process_exits: u64,
    /// Total TCP retransmits detected.
    pub tcp_retransmits: u64,
    /// Total security violations detected.
    pub security_violations: u64,
    /// Total events processed from the ring buffer.
    pub events_processed: u64,
    /// Total events that failed to parse.
    pub events_parse_errors: u64,
    /// Current FD usage ratio (0.0–1.0).
    pub fd_usage_ratio: f64,
    /// Current memory pressure level gauge.
    pub memory_pressure_level: i64,
    /// Current TCP connection count.
    pub tcp_connection_count: i64,
    /// Current open FD count.
    pub fd_count: i64,
}

/// Handle to the running eBPF monitor.
///
/// This handle keeps the monitor alive. When dropped, the shutdown signal
/// is sent and all monitor tasks will exit gracefully.
///
/// # Lifecycle
///
/// 1. Created by [`init()`]
/// 2. Kept alive for the duration of the node's lifetime
/// 3. On drop (or explicit [`shutdown()`](MonitorHandle::shutdown)), all
///    background tasks are cancelled
pub struct MonitorHandle {
    /// Shutdown signal sender. When dropped, all monitor tasks will exit.
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    /// Whether eBPF programs are active (vs userspace fallback).
    ebpf_active: bool,
    /// Optional join handle for the consumer task (eBPF mode).
    _consumer_handle: Option<tokio::task::JoinHandle<()>>,
    /// Optional join handle for the fallback task.
    _fallback_handle: Option<tokio::task::JoinHandle<()>>,
    /// Owns attached programs for the lifetime of the monitor handle.
    #[cfg(feature = "ebpf")]
    _loaded_ebpf: Option<loader::LoadedEbpf>,
    /// Shared metrics reference (for admin API status queries).
    metrics: Arc<EbpfMetrics>,
    /// Shared dispatcher reference (for admin API status queries).
    dispatcher: Arc<ActionDispatcher>,
    /// Namespace identity map for eBPF namespace enforcement.
    /// Provides resolve_identity() for the gateway and register_tid()
    /// for the Supervisor.
    pub namespace_map: Arc<NamespaceMap>,
}

impl MonitorHandle {
    /// Returns `true` if eBPF programs are loaded and active.
    ///
    /// When `false`, the monitor is running in userspace fallback mode
    /// with higher latency (5s polling vs 10ms eBPF).
    pub fn is_ebpf_active(&self) -> bool {
        self.ebpf_active
    }

    /// Get a snapshot of the monitor's current status.
    ///
    /// Returns a [`MonitorStatus`] struct with all key metrics and
    /// operational state, suitable for serialization in admin API
    /// responses.
    pub fn status(&self) -> MonitorStatus {
        MonitorStatus {
            ebpf_active: self.ebpf_active,
            backpressure_active: self.dispatcher.is_backpressure_active(),
            degraded_mode: self.dispatcher.is_degraded(),
            pressure_level: self.dispatcher.last_pressure_level(),
            oom_kills: self.metrics.oom_kills.get(),
            process_exits: self.metrics.process_exits.get(),
            tcp_retransmits: self.metrics.tcp_retransmits.get(),
            security_violations: self.metrics.security_violations.get(),
            events_processed: self.metrics.events_processed.get(),
            events_parse_errors: self.metrics.events_parse_errors.get(),
            fd_usage_ratio: self.metrics.get_fd_usage_ratio(),
            memory_pressure_level: self.metrics.memory_pressure_level.get(),
            tcp_connection_count: self.metrics.tcp_connection_count.get(),
            fd_count: self.metrics.fd_count.get(),
        }
    }

    /// Request graceful shutdown of the monitor.
    ///
    /// Sends the shutdown signal to all background tasks. They will
    /// exit on their next polling cycle (within 10ms for eBPF, 5s for fallback).
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

impl Drop for MonitorHandle {
    fn drop(&mut self) {
        // Signal shutdown on drop
        let _ = self.shutdown_tx.send(true);
    }
}

// ── Monitor Initialization ────────────────────────────────────────────────────

/// Initialize the eBPF monitor subsystem.
///
/// This is the primary entry point for the eBPF monitor. It:
/// 1. Validates the configuration
/// 2. Attempts to load and attach eBPF programs (if the `ebpf` feature is enabled)
/// 3. Falls back to userspace monitoring if eBPF is unavailable
/// 4. Starts the ring buffer consumer (eBPF) or polling loop (fallback)
/// 5. Connects the action dispatcher to process events and trigger recovery
///
/// # Arguments
///
/// - `config`: Monitor configuration (thresholds, program enables)
/// - `metrics`: Prometheus metrics (shared with the action dispatcher)
/// - `dispatcher`: Action dispatcher (handles recovery actions for events)
/// - `node_pid`: PID of the wasm-node process (to filter relevant events)
///
/// # Returns
///
/// A [`MonitorHandle`] that keeps the monitor running. Drop it or call
/// [`shutdown()`](MonitorHandle::shutdown) to stop the monitor.
///
/// # Example
///
/// ```rust,ignore
/// let handle = ebpf_monitor::init(
///     MonitorConfig::from_ebpf_section(&config.ebpf),
///     ebpf_metrics,
///     ebpf_dispatcher,
///     std::process::id(),
/// ).await;
/// ```
pub async fn init(
    config: MonitorConfig,
    metrics: Arc<EbpfMetrics>,
    dispatcher: Arc<ActionDispatcher>,
    node_pid: u32,
) -> MonitorHandle {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Validate configuration
    if let Err(e) = config.validate() {
        warn!(
            error = %e,
            "eBPF monitor configuration has validation errors — \
             some thresholds may be misconfigured"
        );
    }

    info!(
        enabled = config.enabled,
        node_pid,
        programs = config.enabled_program_count(),
        "Initializing eBPF monitor"
    );

    // ── Try eBPF path (feature-gated) ─────────────────────────────────────
    #[cfg(feature = "ebpf")]
    {
        if config.enabled {
            match try_init_ebpf(&config, &metrics, &dispatcher, node_pid).await {
                Ok((consumer_handle, action_tx, mut loaded)) => {
                    info!("eBPF monitor initialized with kernel-level monitoring");

                    // Spawn a watchdog that monitors the consumer task.
                    // If the consumer dies, fall back to userspace monitoring.
                    let watchdog_metrics = metrics.clone();
                    let watchdog_dispatcher = dispatcher.clone();
                    let watchdog_config = config.clone();
                    let watchdog_shutdown = shutdown_rx.clone();
                    let _watchdog_handle = tokio::spawn(async move {
                        let _ = consumer_handle.await;
                        warn!(
                            "eBPF ring buffer consumer exited — \
                             starting userspace fallback monitor"
                        );
                        watchdog_metrics.mark_ebpf_fallback();
                        fallback::run_fallback_monitor(
                            watchdog_config,
                            watchdog_metrics,
                            watchdog_dispatcher,
                            node_pid,
                            watchdog_shutdown,
                        )
                        .await;
                    });

                    let namespace_map = Arc::new(NamespaceMap::from_ebpf(loaded.ns_ebpf.as_mut()));

                    // Wire the namespace map into the dispatcher so that
                    // TidConnection / TidDisconnection events update port→TID bindings.
                    dispatcher.set_namespace_map(namespace_map.clone());

                    // Drop the action_tx sender — the dispatcher loop owns the receiver.
                    // When the consumer stops sending, the dispatcher loop will exit.
                    drop(action_tx);

                    return MonitorHandle {
                        shutdown_tx,
                        ebpf_active: true,
                        _consumer_handle: None, // watchdog manages the task
                        _fallback_handle: None,
                        _loaded_ebpf: Some(loaded),
                        metrics: metrics.clone(),
                        dispatcher: dispatcher.clone(),
                        namespace_map,
                    };
                }
                Err(reason) => {
                    info!(
                        reason,
                        "eBPF initialization skipped — using userspace fallback"
                    );
                }
            }
        } else {
            info!("eBPF monitor disabled by configuration");
        }
    }

    #[cfg(not(feature = "ebpf"))]
    {
        if config.enabled {
            info!(
                "eBPF feature not compiled — running in userspace fallback mode. \
                 Compile with --features ebpf for kernel-level monitoring on Linux."
            );
        }
    }

    // ── Fallback: userspace polling ───────────────────────────────────────
    let fallback_config = config.clone();
    let fallback_metrics = metrics.clone();
    let fallback_dispatcher = dispatcher.clone();
    let fallback_shutdown = shutdown_rx;

    let fallback_handle = tokio::spawn(async move {
        fallback::run_fallback_monitor(
            fallback_config,
            fallback_metrics,
            fallback_dispatcher,
            node_pid,
            fallback_shutdown,
        )
        .await;
    });

    let namespace_map = Arc::new(NamespaceMap::new_fallback());
    dispatcher.set_namespace_map(namespace_map.clone());

    MonitorHandle {
        shutdown_tx,
        ebpf_active: false,
        _consumer_handle: None,
        _fallback_handle: Some(fallback_handle),
        #[cfg(feature = "ebpf")]
        _loaded_ebpf: None,
        metrics,
        dispatcher,
        namespace_map,
    }
}

// ── eBPF Initialization (feature-gated) ────────────────────────────────────────

#[cfg(feature = "ebpf")]
async fn try_init_ebpf(
    config: &MonitorConfig,
    metrics: &Arc<EbpfMetrics>,
    dispatcher: &Arc<ActionDispatcher>,
    node_pid: u32,
) -> Result<
    (
        tokio::task::JoinHandle<()>,
        tokio::sync::mpsc::Sender<MonitorEvent>,
        loader::LoadedEbpf,
    ),
    &'static str,
> {
    // Step 1: Load and attach eBPF programs
    let mut loaded = loader::load_and_attach(config, node_pid)
        .await
        .map_err(|e| {
            warn!(error = %e, "Failed to load eBPF programs");
            "ebpf_load_failed"
        })?
        .ok_or("ebpf_not_available")?;

    if loaded.links.is_empty() {
        return Err("no_ebpf_programs_attached");
    }

    // Step 2: Open every independently compiled object's event ring buffer.
    let mut ring_buffers = Vec::new();
    for monitor in &mut loaded.monitors {
        if let Some(ring_buf) = monitor
            .ebpf
            .take_map("EVENTS")
            .and_then(|map| aya::maps::RingBuf::try_from(map).ok())
        {
            ring_buffers.push((monitor.name, ring_buf));
        }
    }
    if let Some(ns_ebpf) = loaded.ns_ebpf.as_mut() {
        if let Some(ring_buf) = ns_ebpf
            .take_map("EVENTS")
            .and_then(|map| aya::maps::RingBuf::try_from(map).ok())
        {
            ring_buffers.push(("namespace_enforcer", ring_buf));
        }
    }
    if ring_buffers.is_empty() {
        return Err("events_ring_buffers_not_found");
    }

    // Step 3: Mark eBPF as active
    metrics.mark_ebpf_active();

    // Step 4: Start the action dispatcher channel
    let action_tx = start_action_dispatcher(dispatcher.clone(), ConsumerConfig::default());

    // Step 5: Spawn the ring buffer consumer task
    let consumer_metrics = metrics.clone();
    let consumer_action_tx = action_tx.clone();
    let consumer_handle = tokio::spawn(async move {
        consumer::consume_ring_buffers(
            ring_buffers,
            consumer_action_tx,
            consumer_metrics,
            Duration::from_millis(10),
        )
        .await;
    });

    info!(
        programs = loaded.links.len(),
        attached = ?loaded.links,
        "eBPF programs loaded and ring buffer consumer started"
    );

    Ok((consumer_handle, action_tx, loaded))
}

// ── Convenience: Create a dispatcher with the NoopCallbacks ────────────────────

/// Create an `ActionDispatcher` with no-op callbacks.
///
/// This is useful for testing or for scenarios where the platform
/// integration is not yet available. Metrics are still updated.
pub fn noop_dispatcher(metrics: Arc<EbpfMetrics>, node_id: String) -> Arc<ActionDispatcher> {
    Arc::new(ActionDispatcher::new_noop(metrics, node_id))
}

// ── Unit Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::MonitorEvent;
    use crate::common::TidIdentity;
    use prometheus::Registry;

    fn make_test_metrics() -> Arc<EbpfMetrics> {
        let registry = Registry::new();
        Arc::new(EbpfMetrics::new(&registry))
    }

    #[tokio::test]
    async fn test_init_creates_fallback_monitor() {
        let metrics = make_test_metrics();
        let dispatcher = noop_dispatcher(metrics.clone(), "test-node".to_string());
        let config = MonitorConfig::default();

        let handle = init(config, metrics.clone(), dispatcher, std::process::id()).await;

        // Without the ebpf feature (or on non-Linux), should be in fallback mode
        #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
        assert!(!handle.is_ebpf_active());

        // Shutdown should work without error
        handle.shutdown();
    }

    #[tokio::test]
    async fn test_init_disabled_config() {
        let metrics = make_test_metrics();
        let dispatcher = noop_dispatcher(metrics.clone(), "test-node".to_string());
        let config = MonitorConfig {
            enabled: false,
            ..MonitorConfig::default()
        };

        let handle = init(config, metrics.clone(), dispatcher, std::process::id()).await;

        // Disabled config should always be in fallback mode
        assert!(!handle.is_ebpf_active());
    }

    #[tokio::test]
    async fn test_fallback_dispatcher_updates_namespace_map_port_bindings() {
        let metrics = make_test_metrics();
        let dispatcher = noop_dispatcher(metrics.clone(), "test-node".to_string());
        let handle = init(
            MonitorConfig::default(),
            metrics,
            dispatcher,
            std::process::id(),
        )
        .await;

        handle
            .namespace_map
            .register_tid(4242, TidIdentity::new("prod", "payments:v1"))
            .unwrap();

        handle.dispatcher.dispatch(MonitorEvent::TidConnection {
            tid: 4242,
            namespace: "prod".to_string(),
            app_id: "payments:v1".to_string(),
            source_port: 18080,
        });

        let identity = handle
            .namespace_map
            .resolve_identity(18080)
            .expect("port binding should resolve after fallback TidConnection");
        assert_eq!(identity.tid, 4242);
        assert_eq!(identity.namespace, "prod");
        assert_eq!(identity.app_id, "payments:v1");

        handle.dispatcher.dispatch(MonitorEvent::TidDisconnection {
            tid: 4242,
            source_port: 18080,
        });
        assert!(handle.namespace_map.resolve_identity(18080).is_none());

        handle.shutdown();
    }

    #[tokio::test]
    async fn test_monitor_handle_shutdown() {
        let metrics = make_test_metrics();
        let dispatcher = noop_dispatcher(metrics.clone(), "test-node".to_string());
        let config = MonitorConfig::default();

        let handle = init(config, metrics, dispatcher, std::process::id()).await;

        // Shutdown should not panic
        handle.shutdown();

        // Double shutdown should not panic
        handle.shutdown();
    }

    #[tokio::test]
    async fn test_monitor_handle_drop_shuts_down() {
        let metrics = make_test_metrics();
        let dispatcher = noop_dispatcher(metrics.clone(), "test-node".to_string());
        let config = MonitorConfig::default();

        {
            let _handle = init(config, metrics, dispatcher, std::process::id()).await;
            // handle is dropped here, should trigger shutdown
        }

        // Give tasks a moment to respond to shutdown
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[test]
    fn test_noop_dispatcher_creation() {
        let metrics = make_test_metrics();
        let dispatcher = noop_dispatcher(metrics.clone(), "test-node".to_string());

        assert!(!dispatcher.is_backpressure_active());
        assert!(!dispatcher.is_degraded());
        assert_eq!(dispatcher.last_pressure_level(), 0);
    }

    #[test]
    fn test_config_validate_default() {
        let config = MonitorConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_validate_invalid() {
        let config = MonitorConfig {
            fd_soft_limit: 10000,
            fd_hard_limit: 5000,
            ..MonitorConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_config_enabled_program_count() {
        let config = MonitorConfig::default();
        assert_eq!(config.enabled_program_count(), 7);

        let config = MonitorConfig {
            enable_syscall_counter: false,
            ..MonitorConfig::default()
        };
        assert_eq!(config.enabled_program_count(), 6);
    }

    #[tokio::test]
    async fn test_init_with_invalid_config_still_starts() {
        // Even with invalid config, the monitor should start (in fallback mode)
        let metrics = make_test_metrics();
        let dispatcher = noop_dispatcher(metrics.clone(), "test-node".to_string());
        let config = MonitorConfig {
            fd_soft_limit: 0, // Invalid
            fd_hard_limit: 0, // Invalid
            ..MonitorConfig::default()
        };

        let handle = init(config, metrics, dispatcher, std::process::id()).await;
        // Should not panic, just log a warning
        handle.shutdown();
    }

    #[tokio::test]
    async fn test_ebpf_metrics_initial_state() {
        let registry = Registry::new();
        let metrics = EbpfMetrics::new(&registry);

        assert_eq!(metrics.ebpf_active.get(), 0);
        assert_eq!(metrics.oom_kills.get(), 0);
        assert_eq!(metrics.process_exits.get(), 0);
        assert_eq!(metrics.signal_deaths.get(), 0);
        assert_eq!(metrics.tcp_retransmits.get(), 0);
        assert_eq!(metrics.nats_retransmit_events.get(), 0);
        assert_eq!(metrics.security_violations.get(), 0);
        assert_eq!(metrics.events_processed.get(), 0);
        assert_eq!(metrics.events_parse_errors.get(), 0);
    }

    #[tokio::test]
    async fn test_ebpf_metrics_mark_active() {
        let registry = Registry::new();
        let metrics = EbpfMetrics::new(&registry);

        metrics.mark_ebpf_active();
        assert_eq!(metrics.ebpf_active.get(), 1);

        metrics.mark_ebpf_fallback();
        assert_eq!(metrics.ebpf_active.get(), 0);
    }
}

pub mod actions;
pub mod config;
pub mod fallback;
pub mod metrics;

#[cfg(feature = "ebpf")]
pub mod common;

#[cfg(feature = "ebpf")]
pub mod loader;

#[cfg(feature = "ebpf")]
pub mod consumer;

pub use config::MonitorConfig;
#[cfg(feature = "ebpf")]
pub use loader::LoadedEbpf;

/// Public API for initializing the eBPF monitor.
/// Returns `None` if eBPF is not available (unsupported kernel, missing permissions, or feature disabled).
pub async fn init(config: MonitorConfig) -> Option<()> {
    let node_pid = std::process::id();

    #[cfg(feature = "ebpf")]
    {
        match loader::load_and_attach(&config, node_pid).await {
            Ok(Some(loaded)) => {
                let _ = consumer::start_consumer(loaded, config).await;
                Some(())
            }
            Ok(None) => {
                tracing::info!("eBPF monitor disabled by configuration");
                fallback::run_fallback_monitor(config).await;
                None
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load eBPF programs: {}. Falling back to userspace monitoring.",
                    e
                );
                fallback::run_fallback_monitor(config).await;
                None
            }
        }
    }
    #[cfg(not(feature = "ebpf"))]
    {
        tracing::info!("eBPF feature disabled, using userspace fallback monitoring.");
        fallback::run_fallback_monitor(config).await;
        None
    }
}

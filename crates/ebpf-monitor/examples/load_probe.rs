//! Short-lived diagnostic for eBPF object loading and attachment.
//!
//! Run as root from the repository root. All links are detached when this
//! process exits, so the probe does not leave programs attached to the host.

use ebpf_monitor::{loader, MonitorConfig};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("ebpf_monitor=debug,info")),
        )
        .init();

    let loaded = loader::load_and_attach(&MonitorConfig::default(), std::process::id()).await?;
    match loaded {
        Some(loaded) => {
            println!(
                "attached {} eBPF monitor(s): {}",
                loaded.links.len(),
                loaded.links.join(", ")
            );
        }
        None => println!("no eBPF monitors attached; inspect the diagnostics above"),
    }

    Ok(())
}

//! Ring buffer consumer for eBPF events.

use crate::{LoadedEbpf, MonitorConfig};

// TODO: Implement ring buffer consumer and event dispatcher.
pub async fn start_consumer(
    _loaded: LoadedEbpf,
    _config: MonitorConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {})
}

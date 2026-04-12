// crates/messaging/src/publisher.rs
use crate::{events::Event, NatsBus};
use tokio::sync::mpsc;

/// Background task: drains an mpsc channel and publishes events to NATS.
pub async fn run_publisher(bus: NatsBus, mut rx: mpsc::Receiver<Event>) {
    while let Some(event) = rx.recv().await {
        if let Err(e) = bus.publish(&event).await {
            tracing::error!(error = %e, "failed to publish event");
        }
    }
}

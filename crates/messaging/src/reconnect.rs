use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct NatsHealth {
    connected: Arc<AtomicBool>,
    last_connected_at: Arc<AtomicU64>,
    degraded_mode_tx: watch::Sender<bool>,
    degraded_mode_rx: watch::Receiver<bool>,
}

impl NatsHealth {
    pub fn new() -> Self {
        let (degraded_mode_tx, degraded_mode_rx) = watch::channel(false);
        NatsHealth {
            connected: Arc::new(AtomicBool::new(true)),
            last_connected_at: Arc::new(AtomicU64::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            )),
            degraded_mode_tx,
            degraded_mode_rx,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    pub fn is_degraded(&self) -> bool {
        *self.degraded_mode_rx.borrow()
    }

    pub fn degraded_mode_stream(&self) -> watch::Receiver<bool> {
        self.degraded_mode_rx.clone()
    }

    pub fn mark_connected(&self) {
        let was_disconnected = !self.connected.swap(true, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.last_connected_at.store(now, Ordering::Relaxed);

        if was_disconnected {
            let _ = self.degraded_mode_tx.send(false);
            tracing::info!("NATS connection restored — exiting degraded mode");
        }
    }

    pub fn mark_disconnected(&self) {
        let was_connected = self.connected.swap(false, Ordering::Relaxed);
        if was_connected {
            let _ = self.degraded_mode_tx.send(true);
            tracing::warn!("NATS connection lost — entering degraded mode");
        }
    }

    pub fn last_message_age_secs(&self) -> u64 {
        let last = self.last_connected_at.load(Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now.saturating_sub(last)
    }

    pub fn update_last_message_time(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.last_connected_at.store(now, Ordering::Relaxed);
    }
}

impl Default for NatsHealth {
    fn default() -> Self {
        Self::new()
    }
}

pub struct NatsHealthWatcher {
    health: NatsHealth,
    poll_interval: Duration,
}

impl NatsHealthWatcher {
    pub fn new(health: NatsHealth, poll_interval: Duration) -> Self {
        Self {
            health,
            poll_interval,
        }
    }

    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.poll_interval);
            loop {
                interval.tick().await;
                self.health.update_last_message_time();
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nats_health_connected() {
        let health = NatsHealth::new();
        assert!(health.is_connected());
        assert!(!health.is_degraded());
    }

    #[test]
    fn test_nats_health_disconnect() {
        let health = NatsHealth::new();
        health.mark_disconnected();
        assert!(!health.is_connected());
    }

    #[test]
    fn test_nats_health_reconnect() {
        let health = NatsHealth::new();
        health.mark_disconnected();
        assert!(!health.is_connected());
        health.mark_connected();
        assert!(health.is_connected());
    }
}

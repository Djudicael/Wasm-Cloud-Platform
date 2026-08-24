use async_nats::Client;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

const NATS_HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const NATS_HEALTH_PROBE_SUBJECT: &str = "$JS.API.INFO";

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
            connected: Arc::new(AtomicBool::new(false)),
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

    fn record_probe_result(&self, succeeded: bool) {
        if succeeded {
            self.mark_connected();
        } else {
            self.mark_disconnected();
        }
    }

    /// Perform a health check on the NATS connection.
    pub fn check_health(&self) -> common::health::DependencyHealth {
        let connected = self.is_connected();
        let degraded = self.is_degraded();
        let last_msg_age = self.last_message_age_secs();

        let (status, message) = if connected && !degraded {
            (
                common::health::DependencyStatus::Healthy,
                "connected".to_string(),
            )
        } else if connected && degraded {
            (
                common::health::DependencyStatus::Degraded,
                format!(
                    "connected but degraded (last message {}s ago)",
                    last_msg_age
                ),
            )
        } else {
            (
                common::health::DependencyStatus::Unhealthy,
                format!("disconnected (last message {}s ago)", last_msg_age),
            )
        };

        common::health::DependencyHealth {
            name: "nats".to_string(),
            status,
            message,
            latency_ms: None,
            last_check: chrono::Utc::now().to_rfc3339(),
        }
    }
}

impl Default for NatsHealth {
    fn default() -> Self {
        Self::new()
    }
}

pub struct NatsHealthWatcher {
    health: NatsHealth,
    client: Client,
    poll_interval: Duration,
}

impl NatsHealthWatcher {
    pub fn new(health: NatsHealth, client: Client, poll_interval: Duration) -> Self {
        Self {
            health,
            client,
            poll_interval,
        }
    }

    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.poll_interval);
            loop {
                interval.tick().await;
                // A socket flush only proves that the client wrote to the local
                // TCP buffer. Requesting JetStream account information requires
                // a server response and therefore detects a stalled NATS guest.
                let probe = tokio::time::timeout(
                    NATS_HEALTH_PROBE_TIMEOUT,
                    self.client.request(NATS_HEALTH_PROBE_SUBJECT, "".into()),
                )
                .await;
                self.health.record_probe_result(matches!(probe, Ok(Ok(_))));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nats_health_initially_disconnected() {
        let health = NatsHealth::new();
        assert!(!health.is_connected());
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

    #[test]
    fn test_probe_result_drives_disconnect_and_recovery() {
        let health = NatsHealth::new();

        health.record_probe_result(true);
        assert!(health.is_connected());
        assert!(!health.is_degraded());

        health.record_probe_result(false);
        assert!(!health.is_connected());
        assert!(health.is_degraded());

        health.record_probe_result(true);
        assert!(health.is_connected());
        assert!(!health.is_degraded());
    }
}

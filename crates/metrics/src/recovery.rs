use prometheus::{IntGauge, Registry};
use std::sync::Arc;

pub struct RecoveryMetrics {
    pub registry: Arc<Registry>,
    pub nats_disconnected: IntGauge,
    pub corrupted_tables: IntGauge,
    pub nats_last_message_age_secs: IntGauge,
    pub recovery_mode: IntGauge,
}

impl RecoveryMetrics {
    #[allow(clippy::new_without_default)]
    #[allow(clippy::unwrap_used)]
    pub fn new() -> Self {
        let registry = Registry::new();

        // NOTE: Prometheus metric creation/register failures indicate programming bugs
        // (duplicate names, invalid names) that should be caught during development.
        // These unwraps are acceptable because they would panic only on malformed code.

        let nats_disconnected = IntGauge::new(
            "wasm_nats_disconnected",
            "1 if the node is disconnected from NATS, 0 otherwise",
        )
        .unwrap();
        registry
            .register(Box::new(nats_disconnected.clone()))
            .unwrap();

        let corrupted_tables = IntGauge::new(
            "wasm_corrupted_tables",
            "Number of corrupted redb tables detected in the last integrity check",
        )
        .unwrap();
        registry
            .register(Box::new(corrupted_tables.clone()))
            .unwrap();

        let nats_last_message_age_secs = IntGauge::new(
            "wasm_nats_last_message_age_secs",
            "Seconds since the last successful NATS message was received",
        )
        .unwrap();
        registry
            .register(Box::new(nats_last_message_age_secs.clone()))
            .unwrap();

        let recovery_mode = IntGauge::new(
            "wasm_recovery_mode",
            "Current recovery mode: 0=normal, 1=full_rebuild, 2=corruption_detected",
        )
        .unwrap();
        registry.register(Box::new(recovery_mode.clone())).unwrap();

        RecoveryMetrics {
            registry: Arc::new(registry),
            nats_disconnected,
            corrupted_tables,
            nats_last_message_age_secs,
            recovery_mode,
        }
    }

    pub fn set_nats_disconnected(&self, disconnected: bool) {
        self.nats_disconnected.set(if disconnected { 1 } else { 0 });
    }

    pub fn set_corrupted_tables(&self, count: i64) {
        self.corrupted_tables.set(count);
    }

    pub fn set_nats_last_message_age(&self, secs: i64) {
        self.nats_last_message_age_secs.set(secs);
    }

    pub fn set_recovery_mode(&self, mode: i64) {
        self.recovery_mode.set(mode);
    }
}

impl Default for RecoveryMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recovery_metrics_creation() {
        let metrics = RecoveryMetrics::new();
        assert_eq!(metrics.nats_disconnected.get(), 0);
        assert_eq!(metrics.corrupted_tables.get(), 0);
    }

    #[test]
    fn test_set_nats_disconnected() {
        let metrics = RecoveryMetrics::new();
        metrics.set_nats_disconnected(true);
        assert_eq!(metrics.nats_disconnected.get(), 1);
        metrics.set_nats_disconnected(false);
        assert_eq!(metrics.nats_disconnected.get(), 0);
    }

    #[test]
    fn test_set_corrupted_tables() {
        let metrics = RecoveryMetrics::new();
        metrics.set_corrupted_tables(3);
        assert_eq!(metrics.corrupted_tables.get(), 3);
    }

    #[test]
    fn test_set_recovery_mode() {
        let metrics = RecoveryMetrics::new();
        metrics.set_recovery_mode(1);
        assert_eq!(metrics.recovery_mode.get(), 1);
    }
}

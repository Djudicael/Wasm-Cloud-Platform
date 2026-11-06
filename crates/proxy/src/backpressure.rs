// crates/proxy/src/backpressure.rs
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared between Supervisor and Pingora.
/// When false, Pingora rejects new requests with 503.
#[derive(Clone)]
pub struct BackpressureSignal {
    accepting: Arc<AtomicBool>,
    operator_draining: Arc<AtomicBool>,
    operator_cordoned: Arc<AtomicBool>,
}

impl BackpressureSignal {
    pub fn new() -> Self {
        BackpressureSignal {
            accepting: Arc::new(AtomicBool::new(true)),
            operator_draining: Arc::new(AtomicBool::new(false)),
            operator_cordoned: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Called by Pingora on every request. Returns true if the node can accept work.
    pub fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Relaxed) && !self.operator_draining.load(Ordering::Relaxed)
    }
    /// Independent operator fence: clearing it must never clear OOM/critical
    /// backpressure established by the memory monitor.
    pub fn set_operator_draining(&self, draining: bool) {
        self.operator_draining.store(draining, Ordering::Relaxed);
    }

    /// Cordon prevents cold admission while preserving existing warm traffic.
    pub fn set_operator_cordoned(&self, cordoned: bool) {
        self.operator_cordoned.store(cordoned, Ordering::Relaxed);
    }

    pub fn can_cold_start(&self) -> bool {
        self.is_accepting() && !self.operator_cordoned.load(Ordering::Relaxed)
    }

    /// Called by the Supervisor when fuel headroom is exhausted.
    pub fn set_rejecting(&self) {
        self.accepting.store(false, Ordering::Relaxed);
        tracing::warn!("backpressure: node is now REJECTING new requests");
    }

    /// Called by the Supervisor when fuel headroom recovers.
    pub fn set_accepting(&self) {
        self.accepting.store(true, Ordering::Relaxed);
        tracing::info!("backpressure: node is now ACCEPTING requests");
    }
}

impl Default for BackpressureSignal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_and_critical_memory_fences_cannot_clear_each_other() {
        let signal = BackpressureSignal::new();
        signal.set_operator_draining(true);
        signal.set_rejecting();
        signal.set_operator_draining(false);
        assert!(!signal.is_accepting());
        signal.set_operator_draining(true);
        signal.set_accepting();
        assert!(!signal.is_accepting());
        signal.set_operator_draining(false);
        assert!(signal.is_accepting());
    }

    #[test]
    fn cordon_only_fences_cold_admission_and_never_clears_other_fences() {
        let signal = BackpressureSignal::new();
        signal.set_operator_cordoned(true);
        assert!(signal.is_accepting());
        assert!(!signal.can_cold_start());
        signal.set_operator_draining(true);
        signal.set_operator_cordoned(false);
        assert!(!signal.is_accepting());
        assert!(!signal.can_cold_start());
        signal.set_rejecting();
        signal.set_operator_draining(false);
        assert!(!signal.can_cold_start());
        signal.set_accepting();
        assert!(signal.can_cold_start());
    }

    #[test]
    fn test_backpressure_signal() {
        let signal = BackpressureSignal::new();

        // Initially accepting
        assert!(signal.is_accepting());

        // Set to rejecting
        signal.set_rejecting();
        assert!(!signal.is_accepting());

        // Set back to accepting
        signal.set_accepting();
        assert!(signal.is_accepting());
    }

    #[test]
    fn test_backpressure_clone() {
        let signal = BackpressureSignal::new();
        let signal2 = signal.clone();

        signal.set_rejecting();
        assert!(!signal.is_accepting());
        assert!(!signal2.is_accepting()); // Clone shares the same state
    }
}

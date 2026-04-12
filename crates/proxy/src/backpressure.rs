// crates/proxy/src/backpressure.rs
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Shared between Supervisor and Pingora.
/// When false, Pingora rejects new requests with 503.
#[derive(Clone)]
pub struct BackpressureSignal {
    accepting: Arc<AtomicBool>,
}

impl BackpressureSignal {
    pub fn new() -> Self {
        BackpressureSignal {
            accepting: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Called by Pingora on every request. Returns true if the node can accept work.
    pub fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Relaxed)
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

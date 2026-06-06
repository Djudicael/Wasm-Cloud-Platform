use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tracing::{Level, Subscriber};
use tracing_subscriber::Layer;

/// A tracing layer that samples logs at INFO, DEBUG, and TRACE levels.
#[derive(Debug)]
pub struct SamplingLayer {
    pub(crate) info_rate: Arc<AtomicU64>,
    pub(crate) debug_rate: Arc<AtomicU64>,
    pub(crate) trace_rate: Arc<AtomicU64>,
    pub(crate) info_counter: Arc<AtomicU64>,
    pub(crate) debug_counter: Arc<AtomicU64>,
    pub(crate) trace_counter: Arc<AtomicU64>,
}

impl SamplingLayer {
    pub fn new(info_rate: u64, debug_rate: u64, trace_rate: u64) -> Self {
        SamplingLayer {
            info_rate: Arc::new(AtomicU64::new(info_rate.max(1))),
            debug_rate: Arc::new(AtomicU64::new(debug_rate.max(1))),
            trace_rate: Arc::new(AtomicU64::new(trace_rate.max(1))),
            info_counter: Arc::new(AtomicU64::new(0)),
            debug_counter: Arc::new(AtomicU64::new(0)),
            trace_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn set_rates(&self, info: u64, debug: u64, trace: u64) {
        self.info_rate.store(info.max(1), Ordering::Relaxed);
        self.debug_rate.store(debug.max(1), Ordering::Relaxed);
        self.trace_rate.store(trace.max(1), Ordering::Relaxed);
    }
}

impl<S: Subscriber> Layer<S> for SamplingLayer {
    fn enabled(
        &self,
        metadata: &tracing::Metadata<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        match *metadata.level() {
            Level::ERROR | Level::WARN => true,
            Level::INFO => {
                let count = self.info_counter.fetch_add(1, Ordering::Relaxed);
                count.is_multiple_of(self.info_rate.load(Ordering::Relaxed))
            }
            Level::DEBUG => {
                let count = self.debug_counter.fetch_add(1, Ordering::Relaxed);
                count.is_multiple_of(self.debug_rate.load(Ordering::Relaxed))
            }
            Level::TRACE => {
                let count = self.trace_counter.fetch_add(1, Ordering::Relaxed);
                count.is_multiple_of(self.trace_rate.load(Ordering::Relaxed))
            }
        }
    }
}

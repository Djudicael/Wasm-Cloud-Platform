//! Correlate kernel reclaim hints with measured memory availability and PSI.
//! A single reclaim is not an admission decision. Recovery requires fresh samples.
mod source;
#[cfg(test)]
mod tests;
use crate::{actions::MonitorEvent, ActionDispatcher, MonitorConfig};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug)]
pub(super) struct Sample {
    available_pages: u64,
    some: f64,
    full: f64,
    oom: u64,
    high: u64,
}

#[derive(Default)]
pub(crate) struct State {
    level: u32,
    bad: u32,
    healthy: u32,
    last_reclaim: Option<Instant>,
    last_counters: Option<(u64, u64)>,
}
impl State {
    fn critical(&mut self) {
        self.level = 2;
        self.healthy = 0;
        self.bad = 0;
    }
    fn sample(&mut self, sample: Sample, config: &MonitorConfig, now: Instant) -> Option<u32> {
        let (oom, high) = self
            .last_counters
            .replace((sample.oom, sample.high))
            .map(|(o, h)| (sample.oom > o, sample.high > h))
            .unwrap_or((false, false));
        let previous = self.level;
        let reclaim = high
            || self
                .last_reclaim
                .is_some_and(|t| now.saturating_duration_since(t) <= Duration::from_secs(5));
        let stalled = sample.some >= 10. || sample.full >= 1.;
        let bad = sample.available_pages < config.mem_low_threshold_pages || reclaim && stalled;
        let healthy = sample.available_pages
            >= config
                .mem_low_threshold_pages
                .saturating_add(config.mem_low_threshold_pages / 2)
            && sample.some < 5.
            && sample.full < 0.5;
        if oom || sample.available_pages < config.mem_critical_threshold_pages {
            self.critical();
        } else if bad {
            self.healthy = 0;
            self.bad = self.bad.saturating_add(1);
            if self.bad >= 3 && self.level == 0 {
                self.level = 1;
            }
        } else if healthy {
            self.bad = 0;
            self.healthy = self.healthy.saturating_add(1);
            if self.healthy >= 5 {
                self.level = 0;
            }
        } else {
            self.bad = 0;
            self.healthy = 0;
        }
        (previous != self.level).then_some(self.level)
    }
}

impl ActionDispatcher {
    /// Kernel memory notifications are hints, except explicit critical evidence.
    pub(crate) fn dispatch_kernel(&self, event: MonitorEvent) {
        if let MonitorEvent::MemPressure { pressure_level, .. } = &event {
            let mut state = self.memory_state.lock().unwrap_or_else(|e| e.into_inner());
            let count_notification = || {
                self.metrics.events_processed.inc();
                self.metrics
                    .events_by_type
                    .with_label_values(&[event.event_type().as_str()])
                    .inc();
            };
            if *pressure_level == 2 {
                let first = state.level != 2;
                state.critical();
                if first {
                    self.dispatch(event);
                } else {
                    count_notification();
                }
            } else if *pressure_level == 1 {
                count_notification();
                state.last_reclaim = Some(Instant::now());
                self.metrics.memory_reclaim_hints.inc();
            } else if *pressure_level == 0 {
                count_notification();
            } else {
                self.dispatch(event);
            }
            // A kernel LOW notification cannot release a different active cause.
        } else if let MonitorEvent::ProcessExit {
            signal: 9,
            pid,
            tid,
            ..
        } = &event
        {
            let (pid, tid) = (*pid, *tid);
            let mut state = self.memory_state.lock().unwrap_or_else(|e| e.into_inner());
            state.critical();
            self.dispatch(event);
            self.dispatch(MonitorEvent::MemPressure {
                pid,
                tid,
                free_pages: 0,
                reclaim_pages: 0,
                pressure_level: 2,
                anon_pages: 0,
            });
        } else {
            self.dispatch(event);
        }
    }
}

pub(crate) fn spawn(
    dispatcher: Arc<ActionDispatcher>,
    node_pid: u32,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let source = source::Source::for_pid(node_pid);
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut unavailable = false;
        loop {
            tokio::select! {
                _ = interval.tick() => {},
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                    continue;
                }
            }
            if *shutdown.borrow() {
                return;
            }
            if !dispatcher.current_config().enable_mem_pressure {
                continue;
            }
            let sample = source
                .as_ref()
                .map_err(|e| e.to_string())
                .and_then(|s| s.read().map_err(|e| e.to_string()));
            let sample = match sample {
                Ok(sample) => {
                    unavailable = false;
                    dispatcher.metrics.memory_sample_available.set(1);
                    sample
                }
                Err(error) => {
                    dispatcher.metrics.memory_sample_available.set(0);
                    // Missing evidence never counts as a healthy recovery window.
                    let mut state = dispatcher
                        .memory_state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    state.bad = 0;
                    state.healthy = 0;
                    if !unavailable {
                        tracing::warn!(%error, "memory evidence unavailable; retaining pressure state");
                    }
                    unavailable = true;
                    continue;
                }
            };
            let config = dispatcher.current_config();
            let mut state = dispatcher
                .memory_state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(level) = state.sample(sample, &config, Instant::now()) {
                dispatcher.dispatch(MonitorEvent::MemPressure {
                    pid: node_pid,
                    tid: node_pid,
                    free_pages: sample.available_pages,
                    reclaim_pages: 0,
                    pressure_level: level,
                    anon_pages: 0,
                });
            }
        }
    })
}

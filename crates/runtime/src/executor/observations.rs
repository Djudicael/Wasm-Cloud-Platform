//! Runtime measurements, independent of any telemetry backend.
use super::*;

#[derive(Debug, Clone)]
pub struct HttpExecution {
    pub fuel_consumed: u64,
    pub fuel_limit: u64,
    pub ram_bytes: usize,
    pub wall_clock_ms: u64,
    /// Actual guest response status; zero means no response was produced.
    pub status_code: u16,
    pub is_trap: bool,
    /// Bounded classification, never a guest-provided error string.
    pub error: Option<&'static str>,
}

pub type HttpExecutionObserver = Arc<dyn Fn(HttpExecution) + Send + Sync>;

pub(super) type RequestTasks = Arc<std::sync::Mutex<Option<tokio::task::JoinSet<()>>>>;

pub(super) fn spawn_request(
    tasks: &RequestTasks,
    future: impl Future<Output = Result<(), std::io::Error>> + Send + 'static,
) -> tokio::sync::oneshot::Receiver<Result<(), std::io::Error>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    if let Ok(mut tasks) = tasks.lock() {
        if let Some(tasks) = tasks.as_mut() {
            while tasks.try_join_next().is_some() {}
            tasks.spawn(async move {
                let _ = sender.send(future.await);
            });
        }
    }
    receiver
}

pub(super) async fn stop_requests(tasks: &RequestTasks) {
    let tasks = tasks.lock().ok().and_then(|mut slot| slot.take());
    if let Some(mut tasks) = tasks {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
}

/// P2 drops its Store before waiting for the response body. Preserve the measured
/// counters until the body is done; task cancellation also emits exactly once.
pub(super) struct DeferredReport {
    observer: Option<HttpExecutionObserver>,
    sample: Arc<std::sync::Mutex<Option<HttpExecution>>>,
    status: Arc<std::sync::atomic::AtomicU16>,
    started: Instant,
    completed: bool,
}

impl DeferredReport {
    pub fn new(
        observer: Option<HttpExecutionObserver>,
        status: Arc<std::sync::atomic::AtomicU16>,
    ) -> Self {
        Self {
            observer,
            sample: Arc::default(),
            status,
            started: Instant::now(),
            completed: false,
        }
    }
    pub fn capture(&self) -> HttpExecutionObserver {
        let sample = self.sample.clone();
        Arc::new(move |value| {
            if let Ok(mut slot) = sample.lock() {
                *slot = Some(value);
            }
        })
    }
    pub fn finish(&mut self) {
        self.completed = true;
    }
}

impl Drop for DeferredReport {
    fn drop(&mut self) {
        if let (Some(observer), Ok(mut slot)) = (&self.observer, self.sample.lock()) {
            if let Some(mut sample) = slot.take() {
                sample.status_code = self.status.load(std::sync::atomic::Ordering::Relaxed);
                sample.wall_clock_ms = self
                    .started
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX);
                if !self.completed && sample.error.is_none() {
                    sample.error = Some("http_execution_cancelled");
                }
                observer(sample);
            }
        }
    }
}

pub(super) fn accounting(
    observer: Option<HttpExecutionObserver>,
    counters: Arc<PolicyCounters>,
) -> HttpExecutionObserver {
    Arc::new(move |sample| {
        let _ = counters.http_fuel_consumed.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |total| Some(total.saturating_add(sample.fuel_consumed)),
        );
        if let Some(observer) = &observer {
            observer(sample);
        }
    })
}

/// Owns the Store so cancellation can still read its final fuel and memory.
pub(super) struct ObservedStore {
    store: Store<StoreState>,
    observer: Option<HttpExecutionObserver>,
    fuel_limit: u64,
    started: Instant,
    pub status: Arc<std::sync::atomic::AtomicU16>,
    outcome: Option<(bool, bool)>,
}

impl ObservedStore {
    pub fn new(
        store: Store<StoreState>,
        observer: Option<HttpExecutionObserver>,
        fuel_limit: u64,
    ) -> Self {
        Self {
            store,
            observer,
            fuel_limit,
            started: Instant::now(),
            status: Arc::default(),
            outcome: None,
        }
    }

    pub fn finish(&mut self, failed: bool, is_trap: bool) {
        self.outcome = Some((failed, is_trap));
    }
}

impl std::ops::Deref for ObservedStore {
    type Target = Store<StoreState>;
    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

impl std::ops::DerefMut for ObservedStore {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.store
    }
}

impl wasmtime::AsContext for ObservedStore {
    type Data = StoreState;
    fn as_context(&self) -> wasmtime::StoreContext<'_, Self::Data> {
        self.store.as_context()
    }
}

impl wasmtime::AsContextMut for ObservedStore {
    fn as_context_mut(&mut self) -> wasmtime::StoreContextMut<'_, Self::Data> {
        self.store.as_context_mut()
    }
}

impl Drop for ObservedStore {
    fn drop(&mut self) {
        if let Some(observer) = &self.observer {
            let mut sample = measure(&self.store, self.fuel_limit, self.started);
            sample.status_code = self.status.load(std::sync::atomic::Ordering::Relaxed);
            sample.error = match self.outcome {
                None => Some("http_execution_cancelled"),
                Some((true, _)) => Some("http_execution_error"),
                Some((false, _)) => None,
            };
            sample.is_trap = self.outcome.is_some_and(|(_, trap)| trap);
            observer(sample);
        }
    }
}

pub(super) fn measure(
    store: &Store<StoreState>,
    fuel_limit: u64,
    started: Instant,
) -> HttpExecution {
    HttpExecution {
        fuel_consumed: fuel_limit.saturating_sub(read_fuel_remaining(store)),
        fuel_limit,
        ram_bytes: store.data().limiter.current_memory() as usize,
        wall_clock_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        status_code: 0,
        is_trap: false,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuel_accounting_saturates_without_an_external_sink() {
        let counters = Arc::new(PolicyCounters::new());
        counters
            .http_fuel_consumed
            .store(u64::MAX - 2, std::sync::atomic::Ordering::Relaxed);
        let observer = accounting(None, counters.clone());
        observer(HttpExecution {
            fuel_consumed: 5,
            fuel_limit: 10,
            ram_bytes: 0,
            wall_clock_ms: 0,
            status_code: 200,
            is_trap: false,
            error: None,
        });
        assert_eq!(
            counters
                .http_fuel_consumed
                .load(std::sync::atomic::Ordering::Relaxed),
            u64::MAX
        );
    }
}

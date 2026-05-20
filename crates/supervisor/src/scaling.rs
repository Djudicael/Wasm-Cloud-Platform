use crate::Supervisor;
use common::types::AppId;
use messaging::{events::Event, NatsBus};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::time::{interval, Duration};

/// Tracks in-flight requests per instance.
/// When concurrency exceeds threshold, a new instance is spawned.
pub struct ConcurrencyController {
    /// app_id → semaphore (permits = max_concurrent_requests per instance)
    semaphores: Arc<RwLock<HashMap<String, Arc<Semaphore>>>>,
    max_per_instance: usize,
}

impl ConcurrencyController {
    pub fn new(max_per_instance: usize) -> Self {
        ConcurrencyController {
            semaphores: Arc::new(RwLock::new(HashMap::new())),
            max_per_instance,
        }
    }

    /// Try to acquire a slot for an app.
    /// If all slots are taken → trigger scale-up before proceeding.
    pub async fn acquire(
        &self,
        app_id: &AppId,
        supervisor: &Supervisor,
    ) -> tokio::sync::OwnedSemaphorePermit {
        let sem = {
            let mut map = self.semaphores.write().await;
            map.entry(app_id.0.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(self.max_per_instance)))
                .clone()
        };

        match sem.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                // All slots taken: attempt to scale up
                tracing::info!(app = %app_id.0, "concurrency limit reached, scaling up");
                if let Ok(_addr) = supervisor.spawn(app_id).await {
                    // New instance added → expand semaphore
                    let mut map = self.semaphores.write().await;
                    if let Some(existing) = map.get_mut(&app_id.0) {
                        existing.add_permits(self.max_per_instance);
                    }
                }
                // Now wait for a permit (will get one from the new instance's slots)
                sem.acquire_owned().await.expect("semaphore closed")
            }
        }
    }
}

/// Snapshot of this node's current resource usage.
#[derive(Debug, Clone)]
pub struct NodeStats {
    pub cpu_percent: f32,
    pub fuel_per_sec: u64,
    pub total_instances: usize,
    pub app_counts: HashMap<String, usize>,
}

/// Start a background NATS load reporter that broadcasts the load of this node
pub fn start_load_reporter(
    supervisor: Arc<Supervisor>,
    bus: NatsBus,
    node_id: String,
    proxy_address: String,
    fuel_budget_per_sec: u64,
) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(5));
        loop {
            ticker.tick().await;

            // In a full implementation, `node_stats` would compute CPU and active fuel
            if let Ok(stats) = supervisor.node_stats().await {
                let load_event = Event::NodeLoad {
                    node_id: node_id.clone(),
                    cpu_percent: stats.cpu_percent,
                    fuel_budget_used_percent: (stats.fuel_per_sec as f32
                        / fuel_budget_per_sec as f32)
                        * 100.0,
                    active_instances: stats.total_instances as u32,
                    proxy_address: proxy_address.clone(),
                };
                bus.publish(&load_event).await.ok();
            }
        }
    });
}

pub struct FuelAdmissionController {
    /// Maximum fuel units per second across all apps on this node.
    node_budget: u64,
    /// Rolling window of fuel consumed per second.
    fuel_per_sec: Arc<Mutex<VecDeque<(u64, u64)>>>, // (timestamp, fuel)
}

impl FuelAdmissionController {
    pub fn new(node_budget: u64) -> Self {
        Self {
            node_budget,
            fuel_per_sec: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Check if there is capacity to run an execution with `fuel_limit` units.
    pub async fn can_run(&self, fuel_limit: u64) -> bool {
        let now_ts = unix_now_secs();
        let mut window = self.fuel_per_sec.lock().await;

        // Remove samples older than 1 second
        window.retain(|(ts, _)| now_ts.saturating_sub(*ts) < 1);

        let total_fuel_this_second: u64 = window.iter().map(|(_, f)| f).sum();
        total_fuel_this_second + fuel_limit <= self.node_budget
    }

    pub async fn record_execution(&self, fuel_consumed: u64) {
        let mut window = self.fuel_per_sec.lock().await;
        window.push_back((unix_now_secs(), fuel_consumed));
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_fuel_admission_controller() {
        let controller = FuelAdmissionController::new(100);

        // Can run a request that fits in the budget
        assert!(controller.can_run(50).await);

        // Record execution that consumes fuel
        controller.record_execution(80).await;

        // Cannot run another request that would exceed the 100 node_budget (80 + 30 = 110)
        assert!(!controller.can_run(30).await);

        // Can still run a smaller request (80 + 20 = 100)
        assert!(controller.can_run(20).await);
    }
}

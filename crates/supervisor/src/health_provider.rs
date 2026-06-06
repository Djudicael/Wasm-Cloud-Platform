//! Bridge from supervisor pool state into proxy health reporting.

use crate::Supervisor;
use common::health::AppHealthSummary;
use proxy::health::InstanceCountProvider;

impl InstanceCountProvider for Supervisor {
    fn active_instance_count(&self) -> u32 {
        match self.pools.try_read() {
            Ok(pools) => pools.values().map(|p| p.active_count() as u32).sum(),
            Err(_) => 0,
        }
    }

    fn deployed_app_count(&self) -> u32 {
        match self.pools.try_read() {
            Ok(pools) => pools.len() as u32,
            Err(_) => 0,
        }
    }

    fn app_health_summaries(&self) -> Vec<AppHealthSummary> {
        match self.pools.try_read() {
            Ok(pools) => pools
                .iter()
                .map(|(app_id, pool)| {
                    let instances = pool.active_count() as u32;
                    AppHealthSummary {
                        app_id: app_id.clone(),
                        instances,
                        healthy_instances: instances,
                        serving: instances > 0,
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

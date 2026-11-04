use crate::instance::ManagedInstance;
use common::types::{AppConfig, InstanceId, InstanceState};
use runtime::executor::PreparedModule;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

pub struct InstancePool {
    pub(crate) config: AppConfig,
    pub(crate) prepared: Arc<PreparedModule>,
    pub(crate) instances: Vec<ManagedInstance>,
}

impl InstancePool {
    pub fn active_count(&self) -> usize {
        self.instances
            .iter()
            .filter(|i| matches!(i.state, InstanceState::Ready { .. }))
            .count()
    }

    pub fn ready_addrs(&self) -> Vec<SocketAddr> {
        self.instances
            .iter()
            .filter_map(|i| match &i.state {
                InstanceState::Ready { addr } => Some(*addr),
                _ => None,
            })
            .collect()
    }

    pub fn idle_instance_ids(&self, idle_secs: u64) -> Vec<InstanceId> {
        self.instances
            .iter()
            .filter(|i| i.is_idle_for(Duration::from_secs(idle_secs)))
            .map(|i| i.id.clone())
            .collect()
    }

    /// Get the total number of instances in this pool.
    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }
}

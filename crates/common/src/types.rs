use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppId(pub String);
impl AppId {
    pub fn new(name: &str, version: &str) -> Self {
        AppId(format!("{name}:{version}"))
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstanceId(pub Uuid);
impl InstanceId {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        InstanceId(Uuid::new_v4())
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FuelQuota(pub u64);
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct MemoryPages(pub u32);
impl MemoryPages {
    pub fn to_bytes(self) -> usize {
        self.0 as usize * 64 * 1024
    }
}
#[derive(Debug, Clone, Copy)]
pub struct ExtendedLimits {
    pub max_open_fds: u32,
    pub max_fs_write_bytes: u64,
    pub max_net_egress_bytes: u64,
    pub max_outbound_connections: u32,
}

impl Default for ExtendedLimits {
    fn default() -> Self {
        ExtendedLimits {
            max_open_fds: 64,
            max_fs_write_bytes: 50 * 1024 * 1024,
            max_net_egress_bytes: 10 * 1024 * 1024,
            max_outbound_connections: 16,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtendedLimitsConfig {
    pub max_open_fds: Option<u32>,
    pub max_fs_write_bytes: Option<u64>,
    pub max_net_egress_bytes: Option<u64>,
    pub max_outbound_connections: Option<u32>,
}

impl ExtendedLimitsConfig {
    pub fn to_limits(&self) -> ExtendedLimits {
        let defaults = ExtendedLimits::default();
        ExtendedLimits {
            max_open_fds: self.max_open_fds.unwrap_or(defaults.max_open_fds),
            max_fs_write_bytes: self
                .max_fs_write_bytes
                .unwrap_or(defaults.max_fs_write_bytes),
            max_net_egress_bytes: self
                .max_net_egress_bytes
                .unwrap_or(defaults.max_net_egress_bytes),
            max_outbound_connections: self
                .max_outbound_connections
                .unwrap_or(defaults.max_outbound_connections),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub id: AppId,
    pub fuel_quota: FuelQuota,
    pub memory_limit: MemoryPages,
    pub env_vars: Vec<(String, String)>,
    pub port: u16,
    #[serde(default)]
    pub extended_limits: Option<ExtendedLimitsConfig>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InstanceState {
    Starting,
    Ready { addr: std::net::SocketAddr },
    Busy,
    Stopping,
    Stopped,
}

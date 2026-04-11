
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppId(pub String);
impl AppId { pub fn new(name: &str, version: &str) -> Self { AppId(format!("{name}:{version}")) } }
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InstanceId(pub Uuid);
impl InstanceId { #[allow(clippy::new_without_default)] pub fn new() -> Self { InstanceId(Uuid::new_v4()) } }
#[derive(Debug, Clone, Copy, Serialize, Deserialize)] pub struct FuelQuota(pub u64);
#[derive(Debug, Clone, Copy, Serialize, Deserialize)] pub struct MemoryPages(pub u32);
impl MemoryPages { pub fn to_bytes(self) -> usize { self.0 as usize * 64 * 1024 } }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub id: AppId, pub fuel_quota: FuelQuota, pub memory_limit: MemoryPages,
    pub env_vars: Vec<(String, String)>, pub port: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InstanceState { Starting, Ready { addr: std::net::SocketAddr }, Busy, Stopping, Stopped }

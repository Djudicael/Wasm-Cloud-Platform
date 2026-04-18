use hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingRecord {
    pub seq: u64,
    pub prev_hash: String,
    pub tenant_id: String,
    pub app_id: String,
    pub instance_id: String,
    pub node_id: String,
    pub timestamp_ms: u64,
    pub fuel_consumed: u64,
    pub fuel_quota: u64,
    pub ram_bytes: u64,
    pub wall_clock_ms: u64,
    pub status_code: u16,
    pub is_trap: bool,
    pub record_hash: String,
}

impl BillingRecord {
    pub fn compute_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.seq.to_le_bytes());
        hasher.update(self.prev_hash.as_bytes());
        hasher.update(self.tenant_id.as_bytes());
        hasher.update(self.app_id.as_bytes());
        hasher.update(self.instance_id.as_bytes());
        hasher.update(self.node_id.as_bytes());
        hasher.update(self.timestamp_ms.to_le_bytes());
        hasher.update(self.fuel_consumed.to_le_bytes());
        hasher.update(self.fuel_quota.to_le_bytes());
        hasher.update(self.ram_bytes.to_le_bytes());
        hasher.update(self.wall_clock_ms.to_le_bytes());
        hasher.update(self.status_code.to_le_bytes());
        hasher.update([self.is_trap as u8]);
        hex::encode(hasher.finalize())
    }
}

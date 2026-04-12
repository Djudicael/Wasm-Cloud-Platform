pub mod collector;
pub mod exporter;
pub mod nats;
pub mod tracing_setup;

use serde::{Deserialize, Serialize};

/// A single execution record, produced by the Supervisor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSample {
    pub app_id: String,
    pub instance_id: String,
    pub timestamp_ms: u64,
    pub fuel_consumed: u64,
    pub fuel_limit: u64,
    pub ram_bytes: usize,
    pub wall_clock_ms: u64,
    pub status_code: u16, // HTTP response code (200, 500, etc.)
    pub is_trap: bool,
    pub trap_reason: Option<String>,
    pub trace_id: Option<String>,
}

#[cfg(test)]
mod tests;

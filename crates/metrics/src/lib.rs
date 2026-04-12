pub mod collector;
pub mod exporter;
pub mod log_dispatcher;
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

/// A single log line emitted by a Wasm module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmLogRecord {
    /// Which app produced this log.
    pub app_id: String,
    /// Which instance (UUID).
    pub instance_id: String,
    /// "stdout" or "stderr"
    pub stream: String,
    /// ISO-8601 timestamp (added by the Supervisor, not the app).
    pub node_timestamp: String,
    /// The raw log line.
    pub message: String,
    /// If the line is valid JSON, the parsed fields are preserved here.
    pub structured: Option<serde_json::Value>,
    /// Trace ID forwarded from the request context (if injected as TRACE_ID env var).
    pub trace_id: Option<String>,
}

impl WasmLogRecord {
    pub fn from_line(
        app_id: &str,
        instance_id: &str,
        stream: &str,
        line: &[u8],
        trace_id: Option<String>,
    ) -> Self {
        let message = String::from_utf8_lossy(line).to_string();
        // Try to parse as structured JSON (tracing-subscriber with .json() format)
        let structured = serde_json::from_str::<serde_json::Value>(&message).ok();
        WasmLogRecord {
            app_id: app_id.to_string(),
            instance_id: instance_id.to_string(),
            stream: stream.to_string(),
            node_timestamp: chrono::Utc::now().to_rfc3339(),
            message,
            structured,
            trace_id,
        }
    }
}

#[cfg(test)]
mod tests;

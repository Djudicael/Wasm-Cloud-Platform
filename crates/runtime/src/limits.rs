// crates/runtime/src/limits.rs
use common::error::PlatformError;
use common::types::{FuelQuota, MemoryPages};
use wasmtime::{ResourceLimiter, Store};

/// Apply resource limits to a Store before creating an Instance.
pub fn configure_store<T>(store: &mut Store<T>, fuel: FuelQuota) -> Result<(), PlatformError> {
    // Set fuel limit (CPU metering).
    // Every Wasm instruction decrements this counter.
    store
        .set_fuel(fuel.0)
        .map_err(|e| PlatformError::runtime(format!("fuel error: {e}")))?;

    tracing::debug!(fuel = fuel.0, "store fuel limits configured");
    Ok(())
}

/// Read how much fuel remains after execution.
pub fn read_fuel_remaining<T>(store: &Store<T>) -> u64 {
    store.get_fuel().unwrap_or_else(|e| {
        tracing::debug!("Failed to read fuel: {}", e);
        0
    })
}

/// A simple resource limiter that enforces maximum memory pages.
pub struct MemoryLimiter {
    max_memory: u64,
    memory_used: u64,
}

impl std::fmt::Debug for MemoryLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryLimiter")
            .field("max_memory", &self.max_memory)
            .field("memory_used", &self.memory_used)
            .finish()
    }
}

impl MemoryLimiter {
    pub fn new(limit: MemoryPages) -> Self {
        Self {
            max_memory: limit.to_bytes(),
            memory_used: 0,
        }
    }

    pub fn current_memory(&self) -> u64 {
        self.memory_used
    }
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        let desired = desired as u64;
        if desired > self.max_memory {
            return Ok(false); // Refuse memory growth
        }
        self.memory_used = desired;
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        Ok(true) // No table limit for now
    }
}

/// I/O statistics collected during execution.
/// Populated from `PolicyCounters` after the instance finishes running.
#[derive(Debug, Clone)]
pub struct IoStats {
    pub open_fds_peak: u32,
    pub fs_bytes_written: u64,
    pub net_egress_bytes: u64,
    pub outbound_connections: u32,
}

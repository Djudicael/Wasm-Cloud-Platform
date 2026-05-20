// crates/runtime/src/limits.rs
use common::error::PlatformError;
use common::types::{ExtendedLimits, FuelQuota, MemoryPages};
use wasmtime::{ResourceLimiter, Store};

/// Apply resource limits to a Store before creating an Instance.
pub const EPOCH_DEADLINE_TICKS: u64 = 10;

pub fn configure_store<T>(store: &mut Store<T>, fuel: FuelQuota) -> Result<(), PlatformError> {
    // Set fuel limit (CPU metering).
    // Every Wasm instruction decrements this counter.
    store
        .set_fuel(fuel.0)
        .map_err(|e| PlatformError::runtime(format!("fuel error: {e}")))?;

    // Also configure coarse-grained epoch interruption so runaway guests can be
    // interrupted even if fuel quotas are very large.
    store.epoch_deadline_trap();
    store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);

    tracing::debug!(
        fuel = fuel.0,
        epoch_deadline_ticks = EPOCH_DEADLINE_TICKS,
        "store fuel and epoch limits configured"
    );
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
    max_table_elements: u32,
    table_elements: u32,
}

impl std::fmt::Debug for MemoryLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryLimiter")
            .field("max_memory", &self.max_memory)
            .field("memory_used", &self.memory_used)
            .field("max_table_elements", &self.max_table_elements)
            .field("table_elements", &self.table_elements)
            .finish()
    }
}

impl MemoryLimiter {
    pub fn new(limit: MemoryPages, extended_limits: ExtendedLimits) -> Self {
        Self {
            max_memory: limit.to_bytes(),
            memory_used: 0,
            max_table_elements: extended_limits.max_table_elements,
            table_elements: 0,
        }
    }

    pub fn current_memory(&self) -> u64 {
        self.memory_used
    }

    pub fn current_table_elements(&self) -> u32 {
        self.table_elements
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
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        let desired = desired as u32;
        if desired > self.max_table_elements {
            return Ok(false);
        }
        self.table_elements = desired;
        Ok(true)
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

#[cfg(test)]
mod tests {
    use super::MemoryLimiter;
    use common::types::{ExtendedLimits, MemoryPages};
    use wasmtime::ResourceLimiter;

    #[test]
    fn test_memory_limiter_enforces_table_limit() {
        let mut limiter = MemoryLimiter::new(MemoryPages(10), ExtendedLimits::default());
        assert!(limiter.table_growing(0, 1024, None).unwrap());
        assert_eq!(limiter.current_table_elements(), 1024);

        let over_limit = ExtendedLimits {
            max_table_elements: 32,
            ..ExtendedLimits::default()
        };
        let mut limiter = MemoryLimiter::new(MemoryPages(10), over_limit);
        assert!(limiter.table_growing(0, 32, None).unwrap());
        assert!(!limiter.table_growing(32, 33, None).unwrap());
        assert_eq!(limiter.current_table_elements(), 32);
    }
}

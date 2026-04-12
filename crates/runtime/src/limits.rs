// crates/runtime/src/limits.rs
use common::error::PlatformError;
use common::types::{ExtendedLimits, FuelQuota, MemoryPages};
use wasmtime::{ResourceLimiter, Store};

/// Apply resource limits to a Store before creating an Instance.
pub fn configure_store<T>(store: &mut Store<T>, fuel: FuelQuota) -> Result<(), PlatformError> {
    // Set fuel limit (CPU metering).
    // Every Wasm instruction decrements this counter.
    store
        .set_fuel(fuel.0)
        .map_err(|e| PlatformError::Runtime(format!("fuel error: {e}")))?;

    tracing::debug!(fuel = fuel.0, "store fuel limits configured");
    Ok(())
}

/// Read how much fuel remains after execution.
pub fn read_fuel_remaining<T>(store: &Store<T>) -> u64 {
    store.get_fuel().unwrap_or(0)
}

/// A simple resource limiter that enforces maximum memory pages.
pub struct MemoryLimiter {
    max_memory: usize,
    memory_used: usize,
}

impl MemoryLimiter {
    pub fn new(limit: MemoryPages) -> Self {
        Self {
            max_memory: limit.to_bytes(),
            memory_used: 0,
        }
    }
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, anyhow::Error> {
        if desired > self.max_memory {
            return Ok(false); // Refuse memory growth
        }
        self.memory_used = desired;
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: u32,
        _desired: u32,
        _maximum: Option<u32>,
    ) -> Result<bool, anyhow::Error> {
        Ok(true) // No table limit for now
    }
}

pub struct IoResourceTracker {
    limits: ExtendedLimits,
    open_fds: u32,
    fs_bytes_written: u64,
    net_egress_bytes: u64,
    outbound_connections: u32,
}

impl IoResourceTracker {
    pub fn new(limits: ExtendedLimits) -> Self {
        IoResourceTracker {
            limits,
            open_fds: 0,
            fs_bytes_written: 0,
            net_egress_bytes: 0,
            outbound_connections: 0,
        }
    }

    pub fn track_fd_open(&mut self) -> Result<(), PlatformError> {
        if self.open_fds >= self.limits.max_open_fds {
            return Err(PlatformError::Runtime(format!(
                "fd limit reached: {} (max {})",
                self.open_fds, self.limits.max_open_fds
            )));
        }
        self.open_fds += 1;
        Ok(())
    }

    pub fn track_fd_close(&mut self) {
        self.open_fds = self.open_fds.saturating_sub(1);
    }

    pub fn track_fs_write(&mut self, bytes: u64) -> Result<(), PlatformError> {
        self.fs_bytes_written += bytes;
        if self.fs_bytes_written > self.limits.max_fs_write_bytes {
            return Err(PlatformError::Runtime(format!(
                "fs write limit exceeded: {} bytes (max {})",
                self.fs_bytes_written, self.limits.max_fs_write_bytes
            )));
        }
        Ok(())
    }

    pub fn track_net_egress(&mut self, bytes: u64) -> Result<(), PlatformError> {
        self.net_egress_bytes += bytes;
        if self.net_egress_bytes > self.limits.max_net_egress_bytes {
            return Err(PlatformError::Runtime(format!(
                "network egress limit exceeded: {} bytes (max {})",
                self.net_egress_bytes, self.limits.max_net_egress_bytes
            )));
        }
        Ok(())
    }

    pub fn track_outbound_connect(&mut self) -> Result<(), PlatformError> {
        self.outbound_connections += 1;
        if self.outbound_connections > self.limits.max_outbound_connections {
            return Err(PlatformError::Runtime(format!(
                "outbound connection limit exceeded: {} (max {})",
                self.outbound_connections, self.limits.max_outbound_connections
            )));
        }
        Ok(())
    }

    pub fn stats(&self) -> IoStats {
        IoStats {
            open_fds_peak: self.open_fds,
            fs_bytes_written: self.fs_bytes_written,
            net_egress_bytes: self.net_egress_bytes,
            outbound_connections: self.outbound_connections,
        }
    }
}

#[derive(Debug, Clone)]
pub struct IoStats {
    pub open_fds_peak: u32,
    pub fs_bytes_written: u64,
    pub net_egress_bytes: u64,
    pub outbound_connections: u32,
}

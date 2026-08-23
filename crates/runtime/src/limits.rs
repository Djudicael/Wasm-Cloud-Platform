// crates/runtime/src/limits.rs
use common::error::PlatformError;
use common::types::{ExtendedLimits, FuelQuota, MemoryPages};
use std::sync::Arc;
use wasmtime::{ResourceLimiter, Store};

use crate::policy_tracker::{PolicyCounters, PolicyEnforcer};

/// Apply resource limits to a Store before creating an Instance.
///
/// The engine epoch advances every 10 ms. Production requests therefore get a
/// 30-second coarse wall-clock ceiling while fuel remains the fine-grained CPU
/// limit. A 100 ms deadline interrupted legitimate CPU-heavy work such as
/// Argon2 password verification.
#[cfg(not(test))]
pub const EPOCH_DEADLINE_TICKS: u64 = 3_000;

// Keep interruption tests fast without weakening the production deadline.
#[cfg(test)]
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
    policy_counters: Option<Arc<PolicyCounters>>,
}

impl std::fmt::Debug for MemoryLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryLimiter")
            .field("max_memory", &self.max_memory)
            .field("memory_used", &self.memory_used)
            .field("max_table_elements", &self.max_table_elements)
            .field("table_elements", &self.table_elements)
            .field("policy_counters", &self.policy_counters.is_some())
            .finish()
    }
}

impl MemoryLimiter {
    pub fn new(
        limit: MemoryPages,
        extended_limits: ExtendedLimits,
        policy_counters: Option<Arc<PolicyCounters>>,
    ) -> Self {
        Self {
            max_memory: limit.to_bytes(),
            memory_used: 0,
            max_table_elements: extended_limits.max_table_elements,
            table_elements: 0,
            policy_counters,
        }
    }

    pub fn current_memory(&self) -> u64 {
        self.memory_used
    }

    pub fn current_table_elements(&self) -> u32 {
        self.table_elements
    }

    fn record_memory_growth_denied(&self) {
        if let Some(counters) = self.policy_counters.as_ref() {
            counters
                .memory_growth_denied_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn record_table_growth_denied(&self) {
        if let Some(counters) = self.policy_counters.as_ref() {
            counters
                .table_growth_denied_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn record_memory_growth(&self, desired: u64) {
        if let Some(counters) = self.policy_counters.as_ref() {
            counters
                .current_memory_bytes
                .store(desired, std::sync::atomic::Ordering::Release);
            PolicyEnforcer::update_peak_u64(&counters.memory_bytes_peak, desired);
        }
    }

    fn record_table_growth(&self, desired: u32) {
        if let Some(counters) = self.policy_counters.as_ref() {
            counters
                .current_table_elements
                .store(desired, std::sync::atomic::Ordering::Release);
            PolicyEnforcer::update_peak_u32(&counters.table_elements_peak, desired);
        }
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
            self.record_memory_growth_denied();
            return Ok(false); // Refuse memory growth
        }
        self.memory_used = desired;
        self.record_memory_growth(desired);
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
            self.record_table_growth_denied();
            return Ok(false);
        }
        self.table_elements = desired;
        self.record_table_growth(desired);
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
    use crate::policy_tracker::PolicyCounters;
    use common::types::{ExtendedLimits, MemoryPages};
    use std::sync::Arc;
    use wasmtime::ResourceLimiter;

    #[test]
    fn test_memory_limiter_enforces_table_limit() {
        let mut limiter = MemoryLimiter::new(MemoryPages(10), ExtendedLimits::default(), None);
        assert!(limiter.table_growing(0, 1024, None).unwrap());
        assert_eq!(limiter.current_table_elements(), 1024);

        let over_limit = ExtendedLimits {
            max_table_elements: 32,
            ..ExtendedLimits::default()
        };
        let mut limiter = MemoryLimiter::new(MemoryPages(10), over_limit, None);
        assert!(limiter.table_growing(0, 32, None).unwrap());
        assert!(!limiter.table_growing(32, 33, None).unwrap());
        assert_eq!(limiter.current_table_elements(), 32);
    }

    #[test]
    fn test_memory_limiter_updates_policy_counters_authoritatively() {
        let counters = Arc::new(PolicyCounters::new());
        let over_limit = ExtendedLimits {
            max_table_elements: 16,
            ..ExtendedLimits::default()
        };
        let mut limiter = MemoryLimiter::new(MemoryPages(2), over_limit, Some(counters.clone()));

        assert!(limiter.memory_growing(0, 65_536, None).unwrap());
        assert_eq!(
            counters
                .current_memory_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
            65_536
        );
        assert_eq!(
            counters
                .memory_bytes_peak
                .load(std::sync::atomic::Ordering::Relaxed),
            65_536
        );

        assert!(!limiter.memory_growing(65_536, 196_608, None).unwrap());
        assert_eq!(
            counters
                .memory_growth_denied_total
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        assert!(limiter.table_growing(0, 8, None).unwrap());
        assert_eq!(
            counters
                .current_table_elements
                .load(std::sync::atomic::Ordering::Relaxed),
            8
        );
        assert_eq!(
            counters
                .table_elements_peak
                .load(std::sync::atomic::Ordering::Relaxed),
            8
        );

        assert!(!limiter.table_growing(8, 17, None).unwrap());
        assert_eq!(
            counters
                .table_growth_denied_total
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }
}

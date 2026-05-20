//! Policy-aware wrappers for WASI host functions.
//!
//! This module provides functions that intercept WASI operations and enforce
//! per-instance policies before delegating to the real implementation.

use crate::executor::StoreState;
use crate::policy_tracker::PolicyDenied;
use wasmtime::AsContextMut;

/// Check outbound connection policy before a TCP connect.
/// Called from the Supervisor's spawn_blocking wrapper around the Wasm module.
pub fn check_tcp_connect_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
    dest_ip: std::net::IpAddr,
    dest_port: u16,
) -> Result<(), PolicyDenied> {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state
        .policy_enforcer
        .check_outbound_tcp_connect(dest_ip, dest_port)
}

/// Record a successful TCP connection.
pub fn record_tcp_connect(store: &mut impl AsContextMut<Data = StoreState>) {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.record_outbound_connect();
}

/// Record a TCP disconnection.
pub fn record_tcp_disconnect(store: &mut impl AsContextMut<Data = StoreState>) {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.record_outbound_disconnect();
}

/// Atomically check and record egress policy before sending data.
///
/// This replaces the older split check/record pattern to avoid TOCTOU races.
pub fn check_egress_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
    bytes: u64,
) -> Result<(), PolicyDenied> {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.check_and_record_egress(bytes)
}

/// Backward-compatible no-op.
///
/// Egress bytes are now recorded atomically in `check_egress_policy`, so a
/// separate record step would double-count usage.
pub fn record_egress(_store: &mut impl AsContextMut<Data = StoreState>, _bytes: u64) {}

/// Check DNS policy before a name lookup.
pub fn check_dns_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
) -> Result<(), PolicyDenied> {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.check_dns_lookup()
}

/// Check bind policy before binding to a port.
pub fn check_bind_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
    port: u16,
) -> Result<(), PolicyDenied> {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.check_bind(port)
}

/// Check FD open policy before opening a file.
pub fn check_fd_open_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
) -> Result<(), PolicyDenied> {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.check_fd_open()
}

/// Record an FD open.
pub fn record_fd_open(store: &mut impl AsContextMut<Data = StoreState>) {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.record_fd_open();
}

/// Record an FD close.
pub fn record_fd_close(store: &mut impl AsContextMut<Data = StoreState>) {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.record_fd_close();
}

/// Atomically check and record filesystem write policy.
///
/// This replaces the older split check/record pattern to avoid TOCTOU races.
pub fn check_fs_write_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
    bytes: u64,
) -> Result<(), PolicyDenied> {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.check_and_record_fs_write(bytes)
}

/// Backward-compatible no-op.
///
/// Filesystem write bytes are now recorded atomically in `check_fs_write_policy`,
/// so a separate record step would double-count usage.
pub fn record_fs_write(_store: &mut impl AsContextMut<Data = StoreState>, _bytes: u64) {}

#[cfg(test)]
mod tests {
    use super::{check_egress_policy, check_fs_write_policy, record_egress, record_fs_write};
    use crate::{executor::StoreState, limits::MemoryLimiter, policy_tracker::PolicyEnforcer};
    use common::{
        policy::{FilesystemPolicy, InstancePolicy, NetworkPolicy},
        types::MemoryPages,
    };
    use std::sync::atomic::Ordering;
    use wasmtime::component::ResourceTable;
    use wasmtime_wasi::WasiCtxBuilder;

    fn test_store(policy: InstancePolicy) -> wasmtime::Store<StoreState> {
        let engine = wasmtime::Engine::default();
        let state = StoreState {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
            limiter: MemoryLimiter::new(MemoryPages(1), common::types::ExtendedLimits::default()),
            policy_enforcer: PolicyEnforcer::new(policy),
        };
        wasmtime::Store::new(&engine, state)
    }

    #[test]
    fn test_check_egress_policy_records_atomically_and_record_is_noop() {
        let policy = InstancePolicy {
            network: NetworkPolicy {
                max_egress_bytes: 1024,
                ..NetworkPolicy::default()
            },
            filesystem: FilesystemPolicy::default(),
        };
        let mut store = test_store(policy);

        check_egress_policy(&mut store, 100).expect("egress check should succeed");
        assert_eq!(
            store
                .data()
                .policy_enforcer
                .counters
                .egress_bytes
                .load(Ordering::Relaxed),
            100
        );

        record_egress(&mut store, 100);
        assert_eq!(
            store
                .data()
                .policy_enforcer
                .counters
                .egress_bytes
                .load(Ordering::Relaxed),
            100
        );
    }

    #[test]
    fn test_check_fs_write_policy_records_atomically_and_record_is_noop() {
        let policy = InstancePolicy {
            network: NetworkPolicy::default(),
            filesystem: FilesystemPolicy {
                max_fs_write_bytes: 1024,
                ..FilesystemPolicy::default()
            },
        };
        let mut store = test_store(policy);

        check_fs_write_policy(&mut store, 128).expect("fs write check should succeed");
        assert_eq!(
            store
                .data()
                .policy_enforcer
                .counters
                .fs_write_bytes
                .load(Ordering::Relaxed),
            128
        );

        record_fs_write(&mut store, 128);
        assert_eq!(
            store
                .data()
                .policy_enforcer
                .counters
                .fs_write_bytes
                .load(Ordering::Relaxed),
            128
        );
    }
}

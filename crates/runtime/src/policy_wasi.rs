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

/// Check egress policy before sending data.
pub fn check_egress_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
    bytes: u64,
) -> Result<(), PolicyDenied> {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.check_egress(bytes)
}

/// Record egress bytes after a successful send.
pub fn record_egress(store: &mut impl AsContextMut<Data = StoreState>, bytes: u64) {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.record_egress(bytes);
}

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

/// Check filesystem write policy.
pub fn check_fs_write_policy(
    store: &mut impl AsContextMut<Data = StoreState>,
    bytes: u64,
) -> Result<(), PolicyDenied> {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.check_fs_write(bytes)
}

/// Record filesystem write bytes.
pub fn record_fs_write(store: &mut impl AsContextMut<Data = StoreState>, bytes: u64) {
    let mut ctx = store.as_context_mut();
    let state = ctx.data_mut();
    state.policy_enforcer.record_fs_write(bytes);
}

//! Policy enforcement tracker for WASI host functions.
//!
//! This module keeps the public policy enforcement API stable while splitting
//! the implementation into focused files:
//! - `counters` for atomic usage/violation counters
//! - `core` for `PolicyEnforcer` construction and shared helpers
//! - `network` for outbound/DNS/bind enforcement
//! - `filesystem` for FD and filesystem write enforcement
//! - `denied` for the error surface returned to host functions

#[path = "policy_tracker/core.rs"]
mod core;
#[path = "policy_tracker/counters.rs"]
mod counters;
#[path = "policy_tracker/denied.rs"]
mod denied;
#[path = "policy_tracker/filesystem.rs"]
mod filesystem;
#[path = "policy_tracker/network.rs"]
mod network;

pub use core::PolicyEnforcer;
pub use counters::PolicyCounters;
pub use denied::PolicyDenied;

#[cfg(test)]
#[path = "policy_tracker/tests.rs"]
mod tests;

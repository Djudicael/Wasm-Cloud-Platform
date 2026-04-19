//! Common types and constants for eBPF programs.
//! This module is re-exported as `ebpf_monitor_bpf_common` in the bpf crate.

#![no_std]

pub mod common;

pub use common::*;

//! Common types and constants for eBPF programs.
//! This module is re-exported as `ebpf_monitor_bpf_common` in the bpf crate.

#![no_std]

pub mod common;

pub use common::*;

/// Device/inode of the node's PID namespace, supplied before program attach.
#[aya_ebpf::macros::map]
static PID_NAMESPACE: aya_ebpf::maps::Array<[u64; 2]> =
    aya_ebpf::maps::Array::with_max_entries(1, 0);

/// Match the TIDs registered by the supervisor, including in nested PID
/// namespaces such as WSL. An unavailable namespace never falls back to a
/// potentially colliding host TID.
#[inline(always)]
pub fn current_pid_tgid() -> u64 {
    let Some(namespace) = PID_NAMESPACE.get(0) else {
        return 0;
    };
    let mut ids = aya_ebpf::bindings::bpf_pidns_info { pid: 0, tgid: 0 };
    let result = unsafe {
        aya_ebpf::helpers::gen::bpf_get_ns_current_pid_tgid(
            namespace[0],
            namespace[1],
            &mut ids,
            core::mem::size_of_val(&ids) as u32,
        )
    };
    if result != 0 {
        return 0;
    }
    (u64::from(ids.tgid) << 32) | u64::from(ids.pid)
}

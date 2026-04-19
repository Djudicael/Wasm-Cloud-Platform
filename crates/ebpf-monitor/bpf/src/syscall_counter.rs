//! eBPF Program: Syscall Anomaly Detector
//!
//! Monitors syscall activity for Wasm instance threads. Wasm SFI prevents
//! a Wasm module from making direct syscalls — all syscalls go through
//! Wasmtime's WASI layer. But defense in depth means we also monitor at
//! the kernel level. If a Wasm module somehow bypasses Wasmtime (a
//! hypothetical Wasmtime bug), the syscall monitor catches it.
//!
//! **Requires nightly Rust and the `bpfel-unknown-none` target to compile.**
//!
//! # What It Detects
//!
//! - **Syscall rate per PID**: Count syscalls per second for each Wasm
//!   instance thread. An infinite loop that makes syscalls (e.g.,
//!   `clock_gettime` in a tight loop) will show an anomalously high rate.
//! - **Privileged syscalls**: If a Wasm instance thread makes `ptrace`,
//!   `bpf`, `mount`, `setuid`, or other privilege-escalation syscalls,
//!   it's a security incident.
//! - **Unexpected network syscalls**: A Wasm instance that calls `bind()`
//!   on an unauthorized port (not its pre-bound port) is violating the
//!   network policy.
//!
//! # Userspace Action on Syscall Anomaly
//!
//! 1. **Privilege escalation syscall from Wasm instance**: This is a
//!    **critical security incident**. The Wasm SFI boundary has been
//!    bypassed (hypothetical Wasmtime bug). Actions:
//!    - Immediately kill the instance (`JoinHandle::abort()`)
//!    - Log a `SECURITY` alert with the PID, syscall number, and app ID
//!    - Emit `wasm_security_syscall_violation_total` Prometheus counter
//!    - Publish `Event::SecurityIncident` to NATS (all nodes quarantine
//!      this artifact hash)
//!    - Write an audit log entry
//!
//! 2. **High syscall rate**: If a PID exceeds `syscall_rate_limit`
//!    (default: 100,000/sec), it's likely in a tight loop. The
//!    Supervisor reduces the instance's fuel allocation to throttle it,
//!    or kills it if the rate persists for 3 consecutive windows.
//!
//! 3. **execve from Wasm instance**: A Wasm instance should never call
//!    `execve`. This indicates either a Wasmtime bug or a compromised
//!    host. Kill the instance and log a `SECURITY` alert.
//!
//! # Performance Considerations
//!
//! The syscall counter fires on **every syscall** from monitored PIDs.
//! For a node handling 10,000 requests/second with ~100 syscalls per
//! request, that's 1,000,000 events/second. To minimize overhead:
//!
//! - Normal syscalls only increment a per-CPU counter (no ring buffer write)
//! - Suspicious syscalls generate a ring buffer event
//! - The `MONITORED_PIDS` map filters out non-Wasm threads
//! - Per-CPU maps avoid lock contention

#![no_std]
#![no_main]

use aya_ebpf::{
    cty::c_long,
    macros::{map, tracepoint},
    maps::{Array, HashMap, PerCpuHashMap, RingBuf},
    programs::TracePointContext,
};
use aya_log_ebpf::{info, warn};

use ebpf_monitor_bpf_common::{
    EventHeader, EventType, MonitorConfigMap, SyscallCategory, SyscallEvent,
};

/// Configuration map (shared with all eBPF programs).
#[map]
static CONFIG: Array<MonitorConfigMap> = Array::with_max_entries(1, 0);

/// Ring buffer for sending events to userspace.
#[map]
static EVENTS: RingBuf = RingBuf::with_max_entries(512 * 1024, 0); // 512 KB

/// Per-PID syscall count in current sampling window.
/// Key: PID, Value: total syscall count.
/// Per-CPU to avoid lock contention on the fast path.
/// Userspace reads and resets these counters every sampling period.
#[map]
static SYSCALL_COUNTS: PerCpuHashMap<u32, u64> = PerCpuHashMap::with_max_entries(10240, 0);

/// Per-PID suspicious syscall count in current sampling window.
/// Key: PID, Value: suspicious syscall count.
/// Per-CPU for low-contention counting.
#[map]
static SUSPICIOUS_COUNTS: PerCpuHashMap<u32, u64> = PerCpuHashMap::with_max_entries(10240, 0);

/// Set of PIDs that are wasm-node children (populated by process_tracker).
/// Key: PID, Value: marker byte (always 1).
/// Only PIDs in this map (or the node PID itself) are monitored for
/// syscalls. This prevents false positives from Tokio worker threads,
/// NATS subscriber threads, and other non-Wasm threads in the same process.
#[map]
static MONITORED_PIDS: HashMap<u32, u8> = HashMap::with_max_entries(10240, 0);

/// Marker value for MONITORED_PIDS entries.
const MONITORED_PID_MARKER: u8 = 1;

// ── Privileged Syscall Numbers (x86_64) ────────────────────────────────────────
//
// These syscall numbers are specific to the x86_64 architecture.
// On other architectures (aarch64, riscv64), the numbers differ.
// The eBPF program should ideally detect the architecture at runtime,
// but for simplicity we use x86_64 numbers and note that this needs
// to be extended for multi-arch support.
//
// See: https://filippo.io/linux-syscall-table/

/// `ptrace` — process tracing, can be used to inspect/modify other processes.
const SYS_PTRACE: u64 = 101;

/// `bpf` — load eBPF programs, can be used to install malicious eBPF code.
const SYS_BPF: u64 = 321;

/// `mount` — mount a filesystem, can be used to inject malicious files.
const SYS_MOUNT: u64 = 165;

/// `umount2` — unmount a filesystem.
const SYS_UMOUNT: u64 = 166;

/// `setuid` — set user ID, can be used for privilege escalation.
const SYS_SETUID: u64 = 105;

/// `setgid` — set group ID, can be used for privilege escalation.
const SYS_SETGID: u64 = 106;

/// `execve` — execute a program. A Wasm instance should NEVER call this.
const SYS_EXECVE: u64 = 59;

/// `clone` — create a new process. Wasm instances should not spawn processes.
const SYS_CLONE: u64 = 56;

/// `fork` — create a new process (variant). Wasm instances should not fork.
const SYS_FORK: u64 = 57;

/// `vfork` — create a new process (variant). Wasm instances should not vfork.
const SYS_VFORK: u64 = 58;

/// `kill` — send a signal to a process. Can be used to kill other processes.
const SYS_KILL: u64 = 62;

/// `tgkill` — send a signal to a specific thread.
const SYS_TGKILL: u64 = 234;

/// `socket` — create a network socket. Unexpected from Wasm instances.
const SYS_SOCKET: u64 = 41;

/// `bind` — bind a socket to an address. Unexpected from Wasm instances.
const SYS_BIND: u64 = 49;

/// `listen` — listen for connections. Unexpected from Wasm instances.
const SYS_LISTEN: u64 = 50;

/// `connect` — initiate a connection. May be expected via WASI socket API.
const SYS_CONNECT: u64 = 42;

/// Tracepoint: raw_syscalls/sys_enter
///
/// Fires on every syscall entry. We use this to:
/// - Count total syscalls per PID (for rate limiting)
/// - Detect privileged syscalls from Wasm instance threads
/// - Classify syscalls into categories for policy enforcement
///
/// The tracepoint format (from /sys/kernel/debug/tracing/events/raw_syscalls/sys_enter/format):
///   field:int id;            offset:8;  size:4;  signed:1;
///   field:unsigned long args[6]; offset:16; size:48; signed:0;
///
/// Note: aya-ebpf's TracePointContext already skips the common header,
/// so offset 0 here is the first tracepoint-specific field.
#[tracepoint]
pub fn sys_enter(ctx: TracePointContext) -> c_long {
    match try_sys_enter(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_sys_enter(ctx: TracePointContext) -> Result<c_long, c_long> {
    let config = CONFIG.get(0).ok_or(0)?;

    let pid_tgid = unsafe { aya_ebpf::helpers::bpf_get_current_pid_tgid() };
    let pid = pid_tgid as u32;

    // ── Filter: only monitor wasm-node children ─────────────────────────
    // The MONITORED_PIDS map is populated by the process_tracker eBPF
    // program when it sees a new child process of the wasm-node process.
    // We also monitor the node PID itself for defense in depth.
    let is_monitored = pid == config.node_pid
        || unsafe { MONITORED_PIDS.get(&pid).is_some() };

    if !is_monitored {
        return Ok(0);
    }

    // ── Read syscall number ─────────────────────────────────────────────
    // The syscall number is the first field after the common header.
    // On x86_64, it's a 32-bit signed integer at offset 8 of the
    // tracepoint-specific data (after the 8-byte common header).
    let syscall_nr: u64 = unsafe { ctx.read_at(8)? }.ok_or(0)?;

    // ── Increment total syscall count ──────────────────────────────────
    // This is the fast path — every syscall from a monitored PID
    // increments the counter. No ring buffer event is generated
    // for normal syscalls to minimize overhead.
    unsafe {
        if let Some(count) = SYSCALL_COUNTS.get_ptr_mut(&pid) {
            *count += 1;
        } else {
            let _ = SYSCALL_COUNTS.insert(&pid, &1, 0);
        }
    }

    // ── Classify the syscall ─────────────────────────────────────────────
    let category = classify_syscall(syscall_nr);

    // ── Handle suspicious syscalls ──────────────────────────────────────
    if category != SyscallCategory::Normal as u32 {
        // Increment suspicious syscall counter
        let suspicious_count = unsafe {
            if let Some(count) = SUSPICIOUS_COUNTS.get_ptr_mut(&pid) {
                *count += 1;
                *count
            } else {
                let _ = SUSPICIOUS_COUNTS.insert(&pid, &1, 0);
                1
            }
        };

        // Emit an event to userspace for every suspicious syscall.
        // Unlike normal syscalls (which only increment a counter),
        // suspicious syscalls generate ring buffer events so userspace
        // can take immediate action (kill the instance, publish a
        // security incident, etc.).
        let event = SyscallEvent {
            header: EventHeader {
                event_type: EventType::SyscallAnomaly as u32,
                timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
                pid,
                tid: (pid_tgid >> 32) as u32,
            },
            syscall_nr,
            syscall_category: category,
            count_in_window: suspicious_count,
        };
        EVENTS.output(&event, 0);

        // Log at appropriate severity based on category
        match category {
            c if c == SyscallCategory::PrivilegeEscalation as u32 => {
                warn!(
                    &ctx,
                    "SECURITY: Privilege escalation syscall nr={} from pid={}",
                    syscall_nr,
                    pid
                );
            }
            c if c == SyscallCategory::ProcessControl as u32 => {
                warn!(
                    &ctx,
                    "SECURITY: Process control syscall nr={} from pid={}",
                    syscall_nr,
                    pid
                );
            }
            c if c == SyscallCategory::NetworkControl as u32 => {
                info!(
                    &ctx,
                    "Network control syscall nr={} from pid={}",
                    syscall_nr,
                    pid
                );
            }
            _ => {}
        }
    }

    // ── Check syscall rate limit ────────────────────────────────────────
    // If the total syscall count exceeds the configured rate limit,
    // emit an event so userspace can throttle or kill the instance.
    // We check this on every syscall to detect tight loops quickly.
    //
    // Note: The SYSCALL_COUNTS map is per-CPU, so the total count
    // across all CPUs may exceed the limit before we detect it.
    // This is acceptable — we're looking for order-of-magnitude
    // violations (e.g., 10x the limit), not exact enforcement.
    let total_count = unsafe { SYSCALL_COUNTS.get(&pid).copied().unwrap_or(0) };
    if total_count > config.syscall_rate_limit {
        // Emit a high-rate event. We use the SyscallAnomaly event type
        // with the Normal category to distinguish it from actual
        // security incidents.
        let event = SyscallEvent {
            header: EventHeader {
                event_type: EventType::SyscallAnomaly as u32,
                timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
                pid,
                tid: (pid_tgid >> 32) as u32,
            },
            syscall_nr: 0, // No specific syscall — rate limit exceeded
            syscall_category: SyscallCategory::Normal as u32,
            count_in_window: total_count,
        };
        EVENTS.output(&event, 0);

        // Reset the counter after alerting to avoid flooding the
        // ring buffer with duplicate rate-limit events.
        unsafe {
            let _ = SYSCALL_COUNTS.insert(&pid, &0, 0);
        }
    }

    Ok(0)
}

/// Classify a syscall number into a category.
///
/// # Categories
///
/// - **Normal**: Most syscalls (read, write, openat, close, fstat, mmap,
///   mprotect, etc.). These are expected from Wasm instances via WASI.
///
/// - **PrivilegeEscalation**: Syscalls that should never be called from
///   a Wasm instance: ptrace, bpf, mount, umount, setuid, setgid.
///   These indicate a potential sandbox escape.
///
/// - **NetworkControl**: Syscalls related to network socket management:
///   socket, bind, listen. These may be expected in some cases
///   (WASI socket API), but unexpected bind/listen calls are suspicious.
///
/// - **ProcessControl**: Syscalls related to process management:
///   execve, clone, fork, vfork, kill, tgkill. A Wasm instance should
///   never create new processes or send signals to other processes.
fn classify_syscall(syscall_nr: u64) -> u32 {
    match syscall_nr {
        // ── Privilege Escalation ──────────────────────────────────────
        // These syscalls should NEVER be called from a Wasm instance.
        // If they are, it indicates a Wasmtime SFI bypass.
        SYS_PTRACE | SYS_BPF | SYS_MOUNT | SYS_UMOUNT | SYS_SETUID | SYS_SETGID => {
            SyscallCategory::PrivilegeEscalation as u32
        }

        // ── Process Control ──────────────────────────────────────────
        // A Wasm instance should never create processes or send signals.
        // execve is particularly dangerous — it replaces the process
        // image, effectively escaping the Wasm sandbox entirely.
        SYS_EXECVE | SYS_CLONE | SYS_FORK | SYS_VFORK | SYS_KILL | SYS_TGKILL => {
            SyscallCategory::ProcessControl as u32
        }

        // ── Network Control ──────────────────────────────────────────
        // These may be expected via the WASI socket API (connect is
        // used for TCP client connections). However, bind and listen
        // are suspicious — a Wasm instance should not be a server.
        // We classify all of them as NetworkControl and let userspace
        // decide based on the app's configuration.
        SYS_SOCKET | SYS_BIND | SYS_LISTEN | SYS_CONNECT => {
            SyscallCategory::NetworkControl as u32
        }

        // ── Normal ──────────────────────────────────────────────────
        // All other syscalls are classified as Normal. This includes:
        // read(0), write(1), openat(257), close(3), fstat(5),
        // mmap(9), mprotect(10), munmap(11), ioctl(16), access(21),
        // pipe(22), select(23), poll(7), nanosleep(35), clock_gettime(228),
        // exit_group(231), rt_sigreturn(15), etc.
        _ => SyscallCategory::Normal as u32,
    }
}

/// Register a PID as a monitored (wasm-node child) process.
///
/// This function is called by the process_tracker eBPF program when
/// it detects a new child process of the wasm-node process. The
/// syscall_counter then monitors this PID for suspicious syscalls.
///
/// Note: This function is intended to be called from another eBPF
/// program (process_tracker) via a BPF-to-BPF call. However, aya-ebpf
/// does not currently support BPF-to-BPF calls in all cases.
///
/// As a workaround, userspace can also populate the MONITORED_PIDS
/// map by writing to it from the ring buffer consumer when it
/// receives a ProcessExec event.
///
/// This function is exported as a noinline function so it can be
/// called from other eBPF programs if BPF-to-BPF calls are supported.
#[inline(never)]
pub fn register_monitored_pid(pid: u32) {
    unsafe {
        let _ = MONITORED_PIDS.insert(&pid, &MONITORED_PID_MARKER, 0);
    }
}

/// Unregister a PID from the monitored set.
///
/// Called when a child process exits (detected by process_tracker).
/// After unregistration, the syscall counter stops monitoring this PID,
/// which reduces overhead and prevents false positives from recycled PIDs.
#[inline(never)]
pub fn unregister_monitored_pid(pid: u32) {
    unsafe {
        let _ = MONITORED_PIDS.remove(&pid);
    }

    // Also clean up the per-PID counters to free map entries.
    // Note: PerCpuHashMap::remove is not available in all aya-ebpf
    // versions. If it's not available, the counters will be cleaned
    // up lazily when the PID is reused (the old count will be
    // overwritten by the new process's syscalls).
    unsafe {
        let _ = SYSCALL_COUNTS.remove(&pid);
        let _ = SUSPICIOUS_COUNTS.remove(&pid);
    }
}

/// Get the total syscall count for a PID in the current window.
///
/// This is a helper for userspace to read the per-PID syscall counts
/// during the sampling period. Userspace calls this via a BPF
/// program invocation or by reading the map directly.
///
/// Note: This function is primarily for documentation purposes.
/// Userspace should read the SYSCALL_COUNTS map directly using
/// aya's map iteration API.
#[inline(never)]
pub fn get_syscall_count(pid: u32) -> u64 {
    unsafe { SYSCALL_COUNTS.get(&pid).copied().unwrap_or(0) }
}

/// Reset the per-PID syscall counters for a new sampling window.
///
/// Called by userspace at the end of each sampling period (default: 10s).
/// This resets all counters to zero so the next window starts fresh.
///
/// Note: This function is primarily for documentation purposes.
/// Userspace should iterate the SYSCALL_COUNTS and SUSPICIOUS_COUNTS
/// maps and delete all entries (or set them to 0) at the end of each
/// sampling period.
#[inline(never)]
pub fn reset_sampling_window() {
    // In eBPF, we can't iterate over a HashMap and delete all entries.
    // This must be done from userspace by iterating the map and
    // removing each entry. This function is a placeholder that
    // documents the expected behavior.
    //
    // Userspace pseudocode:
    //   for pid in SYSCALL_COUNTS.keys() {
    //       SYSCALL_COUNTS.delete(pid);
    //   }
    //   for pid in SUSPICIOUS_COUNTS.keys() {
    //       SUSPICIOUS_COUNTS.delete(pid);
    //   }
}

// ── Architecture-Specific Syscall Tables ────────────────────────────────────────
//
// The syscall numbers above are for x86_64. On other architectures,
// the numbers differ. When adding multi-arch support, create
// architecture-specific classify_syscall functions and select the
// right one at load time based on the detected architecture.
//
// Example for aarch64:
//   SYS_PTRACE = 117
//   SYS_BPF = 280
//   SYS_MOUNT = 40
//   SYS_UMOUNT = 39
//   SYS_SETUID = 146
//   SYS_SETGID = 144
//   SYS_EXECVE = 221
//   SYS_CLONE = 220
//   SYS_FORK = (not available, use clone)
//   SYS_VFORK = (not available, use clone)
//   SYS_SOCKET = 198
//   SYS_BIND = 200
//   SYS_LISTEN = 201
//   SYS_CONNECT = 203
//
// Example for riscv64:
//   SYS_PTRACE = 117
//   SYS_BPF = 280
//   SYS_MOUNT = 40
//   SYS_UMOUNT = 39
//   SYS_SETUID = 146
//   SYS_SETGID = 144
//   SYS_EXECVE = 221
//   SYS_CLONE = 220
//   SYS_SOCKET = 198
//   SYS_BIND = 200
//   SYS_LISTEN = 201
//   SYS_CONNECT = 203
//
// TODO: Add runtime architecture detection and multi-arch syscall tables.

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}

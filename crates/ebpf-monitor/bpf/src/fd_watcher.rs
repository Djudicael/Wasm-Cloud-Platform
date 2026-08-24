//! eBPF Program: File Descriptor Watcher
//!
//! Monitors file descriptor usage per PID to detect FD exhaustion and leaks.
//! Uses kprobes on `fd_install` and `do_filp_close` to track FD counts.
//!
//! **Requires nightly Rust and the `bpfel-unknown-none` target to compile.**
//!
//! # What It Detects
//!
//! - **FD count per PID**: Track the running count of open file descriptors
//!   for the wasm-node process and each Wasm instance thread.
//! - **Approaching limit**: When FD count exceeds 80% of the soft limit,
//!   emit a warning (`FdLimitApproaching`).
//! - **Leak detection**: If FD count increases monotonically over 3 sampling
//!   windows (30 seconds), userspace flags a potential leak.
//! - **FD type breakdown**: Distinguish between file FDs, socket FDs, and
//!   pipe FDs to identify the source of leaks (noted in the event's fd_type).
//!
//! # Userspace Action on FD Events
//!
//! 1. **Soft limit approaching (80%)**: Log a warning. Emit `wasm_fd_usage_ratio`
//!    Prometheus gauge. If the PID is a Wasm instance, notify the Supervisor
//!    to consider pruning idle instances to free FDs.
//! 2. **Hard limit approaching (95%)**: Critical. The Supervisor must
//!    immediately kill the most idle Wasm instance to free FDs before
//!    `accept()` fails. Activate backpressure to stop accepting new
//!    connections until FD count drops.
//! 3. **FD leak detected**: If FD count increases monotonically over 3
//!    consecutive 10-second windows, log a `SECURITY` alert. The
//!    Supervisor kills the leaking instance and marks it for investigation.

#![no_std]
#![no_main]

use aya_ebpf::{
    cty::c_long,
    macros::{kprobe, map},
    maps::{Array, HashMap, RingBuf},
    programs::ProbeContext,
};
use aya_log_ebpf::warn;

use ebpf_monitor_bpf::{EventHeader, EventType, FdEvent, MonitorConfigMap};

/// Configuration map (shared with all eBPF programs).
#[map]
static CONFIG: Array<MonitorConfigMap> = Array::with_max_entries(1, 0);

/// Ring buffer for sending events to userspace.
#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(512 * 1024, 0); // 512 KB

/// Per-PID FD counter.
/// Key: PID, Value: current open FD count.
#[map]
static FD_COUNT: HashMap<u32, u32> = HashMap::with_max_entries(10240, 0);

/// FD type constants (matching userspace FdType enum).
const FD_TYPE_OTHER: u32 = 3;

/// KProbe: fd_install
///
/// Fires when the kernel installs a file descriptor into a process's
/// FD table. We increment the per-PID counter and check against
/// configured limits.
///
/// Signature: `void fd_install(struct file *file, unsigned int fd)`
#[kprobe]
pub fn fd_install(ctx: ProbeContext) -> c_long {
    match try_fd_install(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_fd_install(ctx: ProbeContext) -> Result<c_long, c_long> {
    let config = CONFIG.get(0).ok_or(0)?;

    let pid_tgid = aya_ebpf::helpers::bpf_get_current_pid_tgid();
    let pid = pid_tgid as u32;

    // fd_install(struct file *file, unsigned int fd)
    // arg0 = file pointer, arg1 = fd number
    let fd: u32 = ctx.arg(1).ok_or(0)?;

    // Determine FD type from the file pointer's f_mode or f_op.
    // In eBPF, reading the file struct is complex and kernel-version-dependent.
    // For simplicity, we classify all FDs as "Other" and let userspace
    // determine the type by reading /proc/<pid>/fd/<fd> symlinks.
    // A more sophisticated approach would read the file struct's
    // f_op to determine if it's a socket, pipe, or regular file.
    let fd_type = FD_TYPE_OTHER;

    // Increment FD count for this PID
    let new_count = unsafe {
        if let Some(count) = FD_COUNT.get_ptr_mut(&pid) {
            *count = (*count).saturating_add(1);
            *count
        } else {
            let _ = FD_COUNT.insert(&pid, &1, 0);
            1
        }
    };

    // ── Check against soft limit (80% warning) ──────────────────────────
    if new_count >= config.fd_soft_limit {
        let event = FdEvent {
            header: EventHeader {
                event_type: EventType::FdLimitApproaching as u32,
                timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
                pid,
                tid: (pid_tgid >> 32) as u32,
            },
            fd,
            fd_type,
            current_fd_count: new_count,
            fd_soft_limit: config.fd_soft_limit,
        };
        let _ = EVENTS.output(&event, 0);

        warn!(
            &ctx,
            "FD soft limit approaching: pid={}, count={}, limit={}",
            pid,
            new_count,
            config.fd_soft_limit
        );
    }

    // ── Check against hard limit (95% critical) ─────────────────────────
    if new_count >= config.fd_hard_limit {
        // This is critical — the process is about to hit RLIMIT_NOFILE.
        // accept() will return EMFILE and the node cannot accept new connections.
        let event = FdEvent {
            header: EventHeader {
                event_type: EventType::FdOpen as u32,
                timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
                pid,
                tid: (pid_tgid >> 32) as u32,
            },
            fd,
            fd_type,
            current_fd_count: new_count,
            fd_soft_limit: config.fd_hard_limit,
        };
        let _ = EVENTS.output(&event, 0);

        warn!(
            &ctx,
            "FD hard limit approaching: pid={}, count={}, limit={}",
            pid,
            new_count,
            config.fd_hard_limit
        );
    }

    Ok(0)
}

/// KProbe: do_filp_close (or filp_close on some kernels)
///
/// Fires when the kernel closes a file. We decrement the per-PID counter.
///
/// Signature: `int do_filp_close(struct file *file, fl_owner_t id)`
/// Or: `int filp_close(struct file *filp, fl_owner_t id)`
///
/// Note: We use `do_filp_close` because `filp_close` is a wrapper that
/// may not always be called. On some kernels, the symbol name differs.
/// If attachment fails, userspace should try `filp_close` as a fallback.
#[kprobe]
pub fn do_filp_close(ctx: ProbeContext) -> c_long {
    match try_do_filp_close(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_do_filp_close(_ctx: ProbeContext) -> Result<c_long, c_long> {
    let pid_tgid = aya_ebpf::helpers::bpf_get_current_pid_tgid();
    let pid = pid_tgid as u32;

    // Decrement FD count for this PID
    unsafe {
        if let Some(count) = FD_COUNT.get_ptr_mut(&pid) {
            if *count > 0 {
                *count -= 1;
            }
        }
    }

    Ok(0)
}

/// KProbe: __fdget (alternative hook point for FD tracking)
///
/// On some kernels, `fd_install` may not be available as a kprobe target.
/// `__fdget` is called when a process looks up an FD by number. We can
/// use this as a secondary tracking mechanism to verify FD counts.
///
/// This is currently disabled (commented out) because it fires on every
/// FD access (including reads/writes), which would be very high frequency.
/// Enable only if `fd_install` kprobe attachment fails.
///
/// #[kprobe]
/// pub fn __fdget(ctx: KProbeContext) -> c_long {
///     match try___fdget(ctx) {
///         Ok(ret) => ret,
///         Err(ret) => ret,
///     }
/// }
///
/// fn try___fdget(_ctx: KProbeContext) -> Result<c_long, c_long> {
///     // No-op: just for tracking purposes
///     Ok(0)
/// }

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}

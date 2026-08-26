//! eBPF Program: Memory Pressure Sentinel
//!
//! Monitors memory pressure events from the kernel's reclaim machinery.
//! Fires **before** the OOM killer, giving the node a window to shed
//! load proactively.
//!
//! **Requires nightly Rust and the `bpfel-unknown-none` target to compile.**
//!
//! # What It Detects
//!
//! - **kswapd activity**: The kernel's background reclaim thread wakes up
//!   when free memory drops below the high watermark. This is the earliest
//!   sign of pressure.
//! - **Direct reclaim**: When `try_to_free_pages` is called from the
//!   allocation path, the system is under significant pressure — allocation
//!   latency increases.
//! - **OOM notification**: The `vmpressure` notifier fires at three levels:
//!   low, medium, critical. At "critical", the OOM killer is about to fire.
//! - **Anonymous page tracking**: Wasm linear memory is anonymous (not
//!   file-backed). Tracking `NR_ANON_PAGES` per cgroup identifies which
//!   tenant is consuming memory.
//!
//! # Graduated Response
//!
//! ```text
//! Pressure Level │ Action
//! ───────────────┼──────────────────────────────────────────────────────────────
//! Low            │ Log info. Emit wasm_memory_pressure_level = 0 metric.
//!                │ No instance action.
//! ───────────────┼──────────────────────────────────────────────────────────────
//! Medium         │ Log warning. Emit wasm_memory_pressure_level = 1 metric.
//!                │ Supervisor prunes all idle instances (idle_timeout = 0).
//!                │ Backpressure signal set to "rejecting" for 30s.
//!                │ No new cold starts until pressure drops.
//! ───────────────┼──────────────────────────────────────────────────────────────
//! Critical       │ Log error. Emit wasm_memory_pressure_level = 2 metric.
//!                │ Supervisor kills the largest Wasm instance (most memory).
//!                │ All non-essential instances killed.
//!                │ Backpressure signal set to "rejecting" indefinitely.
//!                │ NATS event: Event::NodeUnderPressure { node_id }
//!                │ Other nodes stop steering traffic here.
//! ```

#![no_std]
#![no_main]

use aya_ebpf::{
    cty::c_long,
    macros::{kprobe, map, tracepoint},
    maps::{Array, HashMap, PerCpuHashMap, RingBuf},
    programs::{ProbeContext, TracePointContext},
};
use aya_log_ebpf::info;

use ebpf_monitor_bpf::{EventHeader, EventType, MemPressureEvent, MonitorConfigMap};

/// Configuration map (shared with all eBPF programs).
#[map]
static CONFIG: Array<MonitorConfigMap> = Array::with_max_entries(1, 0);

/// Ring buffer for sending events to userspace.
#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(512 * 1024, 0); // 512 KB

/// Last reported pressure level per cgroup (to avoid duplicate events).
/// Key: cgroup ID, Value: last pressure level reported.
/// Per-CPU to avoid lock contention.
#[map]
static LAST_PRESSURE: PerCpuHashMap<u64, u32> = PerCpuHashMap::with_max_entries(256, 0);

/// Last direct-reclaim event per cgroup. Direct reclaim can execute once per
/// page, so emit at most one event every ten seconds without suppressing every
/// later pressure episode for the lifetime of the node.
#[map]
static LAST_RECLAIM_EVENT_NS: HashMap<u64, u64> = HashMap::with_max_entries(256, 0);

const RECLAIM_EVENT_INTERVAL_NS: u64 = 10_000_000_000;

/// Pressure level constants (matching userspace enum).
const PRESSURE_LOW: u32 = 0;
const PRESSURE_MEDIUM: u32 = 1;

/// KProbe: try_to_free_pages
///
/// Fires when the kernel's direct reclaim path is entered. This means
/// the system is under memory pressure — an allocation could not be
/// satisfied from free pages and the kernel must reclaim memory
/// synchronously.
///
/// Signature: `unsigned long try_to_free_pages(struct zonelist *zonelist,
///             int order, gfp_t gfp_mask)`
///
/// The `order` parameter indicates the size of the allocation:
/// - order 0: single page (4 KB) — normal allocation
/// - order 3: 8 contiguous pages (32 KB) — higher-order allocation
///   failing suggests severe fragmentation or pressure
#[kprobe]
pub fn try_to_free_pages(ctx: ProbeContext) -> c_long {
    match try_try_to_free_pages(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_try_to_free_pages(ctx: ProbeContext) -> Result<c_long, c_long> {
    let _config = CONFIG.get(0).ok_or(0)?;

    // Read function arguments: zonelist, order, gfp_mask
    // arg(0) = zonelist pointer (not useful for us)
    // arg(1) = order (allocation order — higher means more pressure)
    // arg(2) = gfp_mask (allocation flags)
    let _zonelist: u64 = ctx.arg(0).ok_or(0)?;
    let order: u32 = ctx.arg(1).ok_or(0)?;
    let _gfp_mask: u32 = ctx.arg(2).ok_or(0)?;

    let cgroup_id = unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() };
    let pid_tgid = aya_ebpf::helpers::bpf_get_current_pid_tgid();
    let pid = (pid_tgid >> 32) as u32;
    let tid = pid_tgid as u32;

    let now = unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() };
    if emit_direct_reclaim(cgroup_id, pid, tid, now) {
        info!(
            &ctx,
            "Memory pressure MEDIUM: direct reclaim order={}, cgroup={}", order, cgroup_id
        );
    }

    Ok(0)
}

/// Tracepoint: vmscan/mm_vmscan_direct_reclaim_begin
///
/// This tracepoint covers both global and memory-cgroup direct reclaim and is
/// more stable than relying on one internal reclaim function name. Keep the
/// kprobe above as an additional compatibility signal; the shared rate limit
/// prevents the two hooks from duplicating events.
#[tracepoint]
pub fn direct_reclaim_begin(ctx: TracePointContext) -> c_long {
    match try_direct_reclaim_begin(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_direct_reclaim_begin(ctx: TracePointContext) -> Result<c_long, c_long> {
    let _config = CONFIG.get(0).ok_or(0)?;
    let order: u32 = unsafe { ctx.read_at(8)? };
    let cgroup_id = unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() };
    let pid_tgid = aya_ebpf::helpers::bpf_get_current_pid_tgid();
    let pid = (pid_tgid >> 32) as u32;
    let tid = pid_tgid as u32;
    let now = unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() };

    if emit_direct_reclaim(cgroup_id, pid, tid, now) {
        info!(
            &ctx,
            "Memory pressure MEDIUM: direct reclaim tracepoint order={}, cgroup={}",
            order,
            cgroup_id
        );
    }

    Ok(0)
}

#[inline(always)]
fn emit_direct_reclaim(cgroup_id: u64, pid: u32, tid: u32, now: u64) -> bool {
    // Direct reclaim proves pressure, but allocation order alone does not
    // prove system-wide critical pressure. Reserve critical classification
    // for vmpressure/OOM evidence.
    let pressure_level = PRESSURE_MEDIUM;

    // Rate-limit direct reclaim without permanently suppressing later
    // pressure episodes for this cgroup.
    let last_event_ns = unsafe {
        LAST_RECLAIM_EVENT_NS
            .get(&cgroup_id)
            .copied()
            .unwrap_or(0)
    };
    if now.saturating_sub(last_event_ns) < RECLAIM_EVENT_INTERVAL_NS {
        // Skip closely spaced callbacks to avoid flooding the ring buffer.
        return false;
    }
    let _ = LAST_RECLAIM_EVENT_NS.insert(&cgroup_id, &now, 0);

    // Emit the event. Userspace will read detailed memory stats
    // from /proc/meminfo and cgroup memory.stat since eBPF cannot
    // easily access those files.
    let event = MemPressureEvent {
        header: EventHeader {
            event_type: EventType::MemPressure as u32,
            _padding: 0,
            timestamp_ns: now,
            pid,
            tid,
        },
        free_pages: 0,    // Userspace reads from /proc/meminfo
        reclaim_pages: 0, // Userspace reads from /proc/meminfo
        pressure_level,
        _padding: 0,
        anon_pages: 0, // Userspace reads from cgroup memory.stat
    };
    let _ = EVENTS.output(&event, 0);

    true
}

/// Tracepoint: vmpressure/vmpressure_level_change
///
/// Fires when the kernel's vmpressure notifier detects a change in
/// memory pressure level. This is a more reliable signal than the
/// kprobe-based approach because it comes directly from the kernel's
/// memory management subsystem.
///
/// The tracepoint format (from /sys/kernel/debug/tracing/events/vmpressure/vmpressure_level_change/format):
///   field:u64 dev;           offset:8;  size:8;  signed:0;
///   field:int level;         offset:16; size:4;  signed:1;
///
/// The `level` field is one of:
/// - 0: low pressure (some reclaim activity)
/// - 1: medium pressure (significant reclaim activity)
/// - 2: critical pressure (OOM killer may fire soon)
#[tracepoint]
pub fn vmpressure_level_change(ctx: TracePointContext) -> c_long {
    match try_vmpressure(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_vmpressure(ctx: TracePointContext) -> Result<c_long, c_long> {
    let _config = CONFIG.get(0).ok_or(0)?;

    let cgroup_id = unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() };
    let pid_tgid = aya_ebpf::helpers::bpf_get_current_pid_tgid();
    let pid = (pid_tgid >> 32) as u32;
    let tid = pid_tgid as u32;

    // Read the pressure level from the tracepoint.
    // The vmpressure_level_change tracepoint has:
    //   arg0: dev (u64, cgroup v2 ID on newer kernels)
    //   arg1: level (int, 0=low, 1=medium, 2=critical)
    //
    // Note: The exact offset depends on the tracepoint format.
    // On most kernels, the level is at offset 8 (after the common header
    // and dev field). We read it as a u32 for simplicity.
    let level: u32 = unsafe { ctx.read_at(8)? };

    // Deduplicate: only send event if the pressure level increased.
    let last = unsafe {
        LAST_PRESSURE
            .get(&cgroup_id)
            .copied()
            .unwrap_or(PRESSURE_LOW)
    };
    if last >= level {
        return Ok(0);
    }
    let _ = LAST_PRESSURE.insert(&cgroup_id, &level, 0);

    let event = MemPressureEvent {
        header: EventHeader {
            event_type: EventType::MemPressure as u32,
            _padding: 0,
            timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
            pid,
            tid,
        },
        free_pages: 0,    // Userspace reads from /proc/meminfo
        reclaim_pages: 0, // Userspace reads from /proc/meminfo
        pressure_level: level,
        _padding: 0,
        anon_pages: 0, // Userspace reads from cgroup memory.stat
    };
    let _ = EVENTS.output(&event, 0);

    info!(
        &ctx,
        "vmpressure level change: cgroup={}, level={}", cgroup_id, level
    );

    Ok(0)
}

/// KProbe: out_of_memory (optional, for OOM notification)
///
/// Fires when the kernel's OOM killer is about to be invoked.
/// This is the last resort — by the time this fires, a process
/// will be killed. We emit a critical pressure event so userspace
/// can take immediate action (kill instances proactively before
/// the kernel does).
///
/// Signature: `bool out_of_memory(struct oom_control *oc)`
///
/// Note: This kprobe is optional and may not be available on all
/// kernels. If attachment fails, userspace should continue without it
/// and rely on the vmpressure and try_to_free_pages events.
///
/// Currently disabled because the OOM killer path is complex and
/// kernel-version-dependent. Enable with caution.
///
/// #[kprobe]
/// pub fn out_of_memory(ctx: KProbeContext) -> c_long {
///     match try_out_of_memory(ctx) {
///         Ok(ret) => ret,
///         Err(ret) => ret,
///     }
/// }
///
/// fn try_out_of_memory(ctx: KProbeContext) -> Result<c_long, c_long> {
///     let cgroup_id = unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() };
///     let pid_tgid = unsafe { aya_ebpf::helpers::bpf_get_current_pid_tgid() };
///     let pid = pid_tgid as u32;
///
///     // Always emit a critical event for OOM — no deduplication
///     let event = MemPressureEvent {
///         header: EventHeader {
///             event_type: EventType::MemPressure as u32,
///             timestamp_ns: unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() },
///             pid,
///             tid: (pid_tgid >> 32) as u32,
///         },
///         free_pages: 0,
///         reclaim_pages: 0,
///         pressure_level: PRESSURE_CRITICAL,
///         anon_pages: 0,
///     };
///     EVENTS.output(&event, 0);
///
///     // Update the LAST_PRESSURE map
///     unsafe {
///         let _ = LAST_PRESSURE.insert(&cgroup_id, &PRESSURE_CRITICAL, 0);
///     }
///
///     warn!(&ctx, "OOM killer invoked: cgroup={}", cgroup_id);
///     Ok(0)
/// }

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}

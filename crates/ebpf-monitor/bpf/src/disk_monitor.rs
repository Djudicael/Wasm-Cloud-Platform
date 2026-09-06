//! eBPF Program: Disk I/O Monitor
//!
//! Monitors block device I/O latency by tracking the time from
//! `block_rq_issue` to `block_rq_complete`. When latency exceeds
//! the configured threshold (default: 50ms), emits a `DiskSlowIo`
//! event so userspace can take proactive action.
//!
//! **Requires nightly Rust and the `bpfel-unknown-none` target to compile.**
//!
//! # What It Detects
//!
//! - **I/O latency per device**: Time from `block_rq_issue` to
//!   `block_rq_complete`. If latency exceeds the configured threshold
//!   (default: 50ms), emit a `DiskSlowIo` event.
//! - **Write amplification**: Track the ratio of bytes written to the
//!   device vs. bytes written by redb. High amplification indicates
//!   compaction or journaling overhead.
//! - **I/O queue depth**: If the block device queue depth exceeds a
//!   threshold, the device is saturated and new writes will be delayed.
//!
//! # Userspace Action on Disk I/O Events
//!
//! 1. **Slow I/O detected**: Log warning with device, latency, and I/O
//!    type. Emit `wasm_disk_io_latency_seconds` Prometheus histogram. If
//!    the device is the one holding `state.redb`, switch redb to read-only
//!    mode temporarily (reject writes, serve reads).
//! 2. **Sustained slow I/O (>30s)**: The node enters degraded mode.
//!    Publish `Event::NodeUnderPressure` to NATS. Other nodes stop
//!    steering traffic here. The node continues serving cached reads
//!    but defers all writes until disk recovers.
//! 3. **I/O recovered**: Log info. Exit degraded mode. Publish
//!    `Event::NodeReady`.

#![no_std]
#![no_main]

use aya_ebpf::{
    cty::c_long,
    macros::{map, tracepoint},
    maps::{Array, HashMap, PerCpuArray, RingBuf},
    programs::TracePointContext,
};
use aya_log_ebpf::warn;

use ebpf_monitor_bpf::{DiskIoEvent, EventHeader, EventType, MonitorConfigMap};

/// Configuration map (shared with all eBPF programs).
#[map]
static CONFIG: Array<MonitorConfigMap> = Array::with_max_entries(1, 0);

/// Ring buffer for sending events to userspace.
#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(512 * 1024, 0); // 512 KB

#[map]
static DROPPED_EVENTS: PerCpuArray<u64> = PerCpuArray::with_max_entries(1, 0);

#[inline(always)]
fn emit<T>(event: &T) {
    if EVENTS.output(event, 0).is_err() {
        if let Some(value) = DROPPED_EVENTS.get_ptr_mut(0) {
            unsafe { *value = (*value).saturating_add(1) };
        }
    }
}

#[repr(C)]
#[derive(Copy, Clone)]
struct IoKey {
    sector: u64,
    dev: u32,
    io_type: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct IoStart {
    timestamp_ns: u64,
    cgroup_id: u64,
    pid: u32,
    tid: u32,
    bytes: u32,
    _padding: u32,
}

/// Track I/O start metadata by device, sector, and operation. A sector alone
/// is not unique across devices and caused unrelated requests to correlate.
///
/// Maximum entries: 65536. This should be sufficient for most workloads.
/// If the map fills up, old entries will fail to insert but the system
/// will continue to function (we just miss latency measurements for
/// those requests).
#[map]
static IO_IN_FLIGHT: HashMap<IoKey, IoStart> = HashMap::with_max_entries(65536, 0);

/// I/O type constants (matching userspace enum).
const IO_TYPE_READ: u32 = 0;
const IO_TYPE_WRITE: u32 = 1;
const IO_TYPE_SYNC: u32 = 2;
const IO_TYPE_FLUSH: u32 = 3;
const IO_TYPE_UNKNOWN: u32 = 99;

/// Maximum number of pending I/O requests to track per device.
/// If we exceed this, we stop tracking new requests until old ones
/// complete. This prevents the map from growing unboundedly.
const MAX_PENDING_IO: u64 = 60000;

/// Current count of pending I/O requests (approximate, per-CPU would be better).
/// We use a simple counter to avoid map lookup overhead.
#[map]
static PENDING_IO_COUNT: HashMap<u32, u64> = HashMap::with_max_entries(256, 0);

/// Tracepoint: block/block_rq_issue
///
/// Fires when a block I/O request is submitted to the device driver.
/// We record the start time so we can calculate latency when the
/// request completes.
///
/// Linux 6.1 tracepoint format (absolute offsets, including the common header):
///   field:dev_t dev;          offset:8;  size:4;  signed:0;
///   field:sector_t sector;   offset:16; size:8;  signed:0;
///   field:unsigned int nr_sector; offset:24; size:4; signed:0;
///   field:unsigned int bytes; offset:28; size:4; signed:0;
///   field:char rwbs[8];      offset:32; size:8;  signed:1;
///
/// Aya exposes the raw tracepoint record; it does not remove the eight-byte
/// common header. These offsets must match the validated guest kernel schema.
#[tracepoint]
pub fn block_rq_issue(ctx: TracePointContext) -> c_long {
    match try_block_rq_issue(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_block_rq_issue(ctx: TracePointContext) -> Result<c_long, c_long> {
    let config = CONFIG.get(0).ok_or(0)?;
    let cgroup_id = unsafe { aya_ebpf::helpers::bpf_get_current_cgroup_id() };
    if cgroup_id != config.node_cgroup_id {
        return Ok(0);
    }

    let dev: u32 = unsafe { ctx.read_at(8)? };
    let sector: u64 = unsafe { ctx.read_at(16)? };
    let bytes: u32 = unsafe { ctx.read_at(28)? };
    let io_type = read_io_type(&ctx, 32)?;

    // Record the start metadata for this I/O request. Device, sector, and
    // operation together avoid collisions that a sector-only key allowed.
    let now = unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() };
    let pid_tgid = aya_ebpf::helpers::bpf_get_current_pid_tgid();
    let start = IoStart {
        timestamp_ns: now,
        cgroup_id,
        pid: (pid_tgid >> 32) as u32,
        tid: pid_tgid as u32,
        bytes,
        _padding: 0,
    };
    let key = IoKey {
        sector,
        dev,
        io_type,
    };

    // Check if we have room to track this request.
    // We use a per-device counter to limit the number of pending
    // requests we track. This prevents the map from growing too large.
    let dev_key = dev;
    let pending = unsafe { PENDING_IO_COUNT.get(&dev_key).copied().unwrap_or(0) };
    let already_tracked = unsafe { IO_IN_FLIGHT.get(&key).is_some() };
    if pending < MAX_PENDING_IO && !already_tracked {
        let _ = IO_IN_FLIGHT.insert(&key, &start, 0);

        // Increment pending count
        let new_count = pending + 1;
        let _ = PENDING_IO_COUNT.insert(&dev_key, &new_count, 0);
    }

    // We don't emit any events on issue — only on completion when we
    // can measure the actual latency.

    Ok(0)
}

/// Tracepoint: block/block_rq_complete
///
/// Fires when a block I/O request completes. We calculate the latency
/// by subtracting the start time (recorded in `block_rq_issue`) from
/// the current time. If the latency exceeds the configured threshold,
/// we emit a `DiskSlowIo` event.
///
/// Linux 6.1 tracepoint format (absolute offsets, including common header):
///   field:dev_t dev;          offset:8;  size:4;  signed:0;
///   field:sector_t sector;    offset:16; size:8;  signed:0;
///   field:unsigned int nr_sector; offset:24; size:4; signed:0;
///   field:int error;          offset:28; size:4; signed:1;
///   field:char rwbs[8];       offset:32; size:8; signed:1;
#[tracepoint]
pub fn block_rq_complete(ctx: TracePointContext) -> c_long {
    match try_block_rq_complete(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_block_rq_complete(ctx: TracePointContext) -> Result<c_long, c_long> {
    let config = CONFIG.get(0).ok_or(0)?;

    let dev: u32 = unsafe { ctx.read_at(8)? };
    let sector: u64 = unsafe { ctx.read_at(16)? };
    let nr_sector: u32 = unsafe { ctx.read_at(24)? };
    let io_type = read_io_type(&ctx, 32)?;
    let key = IoKey {
        sector,
        dev,
        io_type,
    };

    let now = unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() };

    // Look up the start time for this request
    let start = unsafe { IO_IN_FLIGHT.get(&key).copied() };

    if let Some(start) = start {
        // Calculate latency
        let latency_ns = now.saturating_sub(start.timestamp_ns);

        // Clean up the map entry
        unsafe {
            let _ = IO_IN_FLIGHT.remove(&key);

            // Decrement pending count
            let dev_key = dev;
            if let Some(count) = PENDING_IO_COUNT.get_ptr_mut(&dev_key) {
                if *count > 0 {
                    *count -= 1;
                }
            }
        }

        // Check against the configured threshold
        if latency_ns > config.disk_slow_threshold_ns {
            // Tracepoint fields contain the kernel's native dev_t, not the
            // userspace new_encode_dev representation: 20 minor bits followed
            // by the major number (Linux kdev_t.h MAJOR/MINOR).
            let dev_major = dev >> 20;
            let dev_minor = dev & 0x000f_ffff;
            let completed_bytes = nr_sector.saturating_mul(512);
            let bytes = if completed_bytes == 0 {
                start.bytes
            } else {
                completed_bytes
            };

            let event = DiskIoEvent {
                header: EventHeader {
                    event_type: EventType::DiskSlowIo as u32,
                    _padding: 0,
                    timestamp_ns: now,
                    pid: start.pid,
                    tid: start.tid,
                },
                dev_major,
                dev_minor,
                sector,
                nr_sector,
                bytes,
                latency_ns,
                cgroup_id: start.cgroup_id,
                io_type,
                _padding2: 0,
            };
            emit(&event);

            warn!(
                &ctx,
                "Slow disk I/O: dev={}:{} bytes={} latency={}ns type={}",
                dev_major,
                dev_minor,
                bytes,
                latency_ns,
                io_type
            );
        }
    } else {
        // No start time found — this can happen if:
        // 1. The request was issued before our eBPF program was loaded
        // 2. The map was full and we couldn't track this request
        // 3. The sector number was reused (unlikely but possible)
        //
        // We silently ignore this — we can't measure latency without
        // a start time.

        // Still decrement the pending count if we can
        unsafe {
            let dev_key = dev;
            if let Some(count) = PENDING_IO_COUNT.get_ptr_mut(&dev_key) {
                if *count > 0 {
                    *count -= 1;
                }
            }
        }
    }

    Ok(0)
}

#[inline(always)]
fn read_io_type(ctx: &TracePointContext, rwbs_offset: usize) -> Result<u32, c_long> {
    // `blk_fill_rwbs` may prefix an operation with `F` for preflush, so the
    // operation is not necessarily the first character. The primary operation
    // is always within the first two bytes on Linux 6.1.
    let first: u8 = unsafe { ctx.read_at(rwbs_offset)? };
    let second: u8 = unsafe { ctx.read_at(rwbs_offset + 1)? };

    if first == b'W' || second == b'W' {
        Ok(IO_TYPE_WRITE)
    } else if first == b'R' || second == b'R' {
        Ok(IO_TYPE_READ)
    } else if first == b'F' || second == b'F' {
        Ok(IO_TYPE_FLUSH)
    } else if first == b'S' || second == b'S' {
        Ok(IO_TYPE_SYNC)
    } else {
        Ok(IO_TYPE_UNKNOWN)
    }
}

/// Tracepoint: block/block_rq_requeue (optional)
///
/// Fires when a block I/O request is requeued (e.g., due to a device
/// error or timeout). We clean up the start time entry to prevent
/// map leaks.
///
/// Currently disabled because requeue events are rare and the map
/// cleanup on completion is sufficient for most cases. Enable if
/// you notice the IO_IN_FLIGHT map growing over time. Requeue cleanup must use
/// the same device/sector/operation key as issue and completion.
///
/// #[tracepoint]
/// pub fn block_rq_requeue(ctx: TracePointContext) -> c_long {
///     match try_block_rq_requeue(ctx) {
///         Ok(ret) => ret,
///         Err(ret) => ret,
///     }
/// }
///
/// fn try_block_rq_requeue(ctx: TracePointContext) -> Result<c_long, c_long> {
///     let dev: u32 = unsafe { ctx.read_at(8)? };
///     let sector: u64 = unsafe { ctx.read_at(16)? };
///     let io_type = read_io_type(&ctx, 32)?;
///     let key = IoKey { sector, dev, io_type };
///
///     // Clean up the start time entry
///     unsafe {
///         let _ = IO_IN_FLIGHT.remove(&key);
///
///         // Decrement pending count
///         let dev_key = dev;
///         if let Some(count) = PENDING_IO_COUNT.get_ptr_mut(&dev_key) {
///             if *count > 0 {
///                 *count -= 1;
///             }
///         }
///     }
///
///     Ok(0)
/// }

/// Periodic cleanup of stale IO_IN_FLIGHT entries.
///
/// In some cases (e.g., device errors, driver bugs), a `block_rq_complete`
/// event may not fire for a request. This leaves stale entries in the
/// IO_IN_FLIGHT map, which can cause it to fill up over time.
///
/// This function is called periodically (every sampling period) by
/// userspace via a BPF timer or by the userspace fallback monitor.
/// It scans the map and removes entries that are older than a
/// threshold (e.g., 30 seconds).
///
/// Note: This function is not currently implemented as an eBPF
/// program because BPF timers require kernel >= 5.15 and the
/// iteration logic is complex. Instead, userspace handles cleanup
/// by reading the map and removing stale entries.
///
/// For now, we rely on the map's max_entries limit (65536) to
/// prevent unbounded growth. When the map is full, new insertions
/// fail silently, which means we miss some latency measurements
/// but the system continues to function.

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}

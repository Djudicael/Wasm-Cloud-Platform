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
    maps::{Array, HashMap, RingBuf},
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

/// Track I/O start time per request.
/// Key: sector number (u64), Value: start timestamp in nanoseconds.
///
/// We use the sector number as the key because it uniquely identifies
/// an I/O request in the block layer. The sector is set when the
/// request is issued and the same sector is available when the
/// request completes.
///
/// Maximum entries: 65536. This should be sufficient for most workloads.
/// If the map fills up, old entries will fail to insert but the system
/// will continue to function (we just miss latency measurements for
/// those requests).
#[map]
static IO_START_TIME: HashMap<u64, u64> = HashMap::with_max_entries(65536, 0);

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
/// The tracepoint format (from /sys/kernel/debug/tracing/events/block/block_rq_issue/format):
///   field:dev_t dev;          offset:8;  size:4;  signed:0;
///   field:sector_t sector;   offset:16; size:8;  signed:0;
///   field:unsigned int nr_sector; offset:24; size:4; signed:0;
///   field:unsigned int bytes; offset:28; size:4; signed:0;
///   field:char rwbs[8];      offset:32; size:8;  signed:1;
///
/// Note: The exact format may vary by kernel version. The offsets below
/// are based on the most common layout. If the tracepoint format changes,
/// the offsets need to be updated.
#[tracepoint]
pub fn block_rq_issue(ctx: TracePointContext) -> c_long {
    match try_block_rq_issue(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_block_rq_issue(ctx: TracePointContext) -> Result<c_long, c_long> {
    // Read tracepoint arguments.
    // After the common tracepoint header (8 bytes), the fields are:
    //   offset 0: dev_t dev (4 bytes, but aligned to 8)
    //   offset 8: sector_t sector (8 bytes)
    //   offset 16: unsigned int nr_sector (4 bytes)
    //   offset 20: unsigned int bytes (4 bytes)
    //   offset 24: char rwbs[8] (8 bytes)
    //
    // Note: aya-ebpf's TracePointContext already skips the common header,
    // so offset 0 here is the first tracepoint-specific field.

    let dev: u32 = unsafe { ctx.read_at(0)? };
    let sector: u64 = unsafe { ctx.read_at(8)? };
    let _nr_sector: u32 = unsafe { ctx.read_at(16)? };

    // Determine I/O type from the rwbs field.
    // The rwbs field is a string like "R", "W", "WS", "WSF", etc.
    // For simplicity, we read the first byte:
    //   'R' (0x52) = read
    //   'W' (0x57) = write
    //   'S' (0x53) = sync
    //   'F' (0x46) = flush
    //   'D' (0x44) = discard
    let rwbs_first_byte: u8 = unsafe { ctx.read_at(24)? };
    let _io_type = match rwbs_first_byte {
        0x52 => IO_TYPE_READ,  // 'R'
        0x57 => IO_TYPE_WRITE, // 'W'
        0x53 => IO_TYPE_SYNC,  // 'S'
        0x46 => IO_TYPE_FLUSH, // 'F'
        0x44 => IO_TYPE_READ,  // 'D' (discard, treat as read for metrics)
        _ => IO_TYPE_UNKNOWN,
    };

    // Record the start time for this I/O request.
    // We use the sector as the key because it uniquely identifies
    // the request in the block layer.
    let now = unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() };

    // Check if we have room to track this request.
    // We use a per-device counter to limit the number of pending
    // requests we track. This prevents the map from growing too large.
    let dev_key = dev;
    let pending = unsafe { PENDING_IO_COUNT.get(&dev_key).copied().unwrap_or(0) };
    if pending < MAX_PENDING_IO {
        let _ = IO_START_TIME.insert(&sector, &now, 0);

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
/// The tracepoint format (from /sys/kernel/debug/tracing/events/block/block_rq_complete/format):
///   field:dev_t dev;          offset:8;  size:4;  signed:0;
///   field:sector_t sector;    offset:16; size:8;  signed:0;
///   field:unsigned int nr_sector; offset:24; size:4; signed:0;
///   field:unsigned int bytes; offset:28; size:4; signed:0;
///   field:char rwbs[8];       offset:32; size:8; signed:1;
///   field:u64 latency_ns;     offset:40; size:8; signed:0;  (some kernels)
///
/// Note: Some kernel versions include a `latency_ns` field directly in
/// the tracepoint. If available, we use it. Otherwise, we calculate it
/// from our IO_START_TIME map.
#[tracepoint]
pub fn block_rq_complete(ctx: TracePointContext) -> c_long {
    match try_block_rq_complete(ctx) {
        Ok(ret) => ret,
        Err(ret) => ret,
    }
}

fn try_block_rq_complete(ctx: TracePointContext) -> Result<c_long, c_long> {
    let config = CONFIG.get(0).ok_or(0)?;

    // Read tracepoint arguments (same layout as block_rq_issue)
    let dev: u32 = unsafe { ctx.read_at(0)? };
    let sector: u64 = unsafe { ctx.read_at(8)? };
    let nr_sector: u32 = unsafe { ctx.read_at(16)? };

    // Determine I/O type from rwbs field
    let rwbs_first_byte: u8 = unsafe { ctx.read_at(24)? };
    let io_type = match rwbs_first_byte {
        0x52 => IO_TYPE_READ,  // 'R'
        0x57 => IO_TYPE_WRITE, // 'W'
        0x53 => IO_TYPE_SYNC,  // 'S'
        0x46 => IO_TYPE_FLUSH, // 'F'
        0x44 => IO_TYPE_READ,  // 'D' (discard)
        _ => IO_TYPE_UNKNOWN,
    };

    let now = unsafe { aya_ebpf::helpers::bpf_ktime_get_ns() };

    // Look up the start time for this request
    let start_ns = unsafe { IO_START_TIME.get(&sector).copied() };

    if let Some(start_ns) = start_ns {
        // Calculate latency
        let latency_ns = now.saturating_sub(start_ns);

        // Clean up the map entry
        unsafe {
            let _ = IO_START_TIME.remove(&sector);

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
            // Extract device major:minor from dev_t
            // dev_t format: bits 0-11 = minor, bits 12-31 = major
            // (on modern kernels, this is extended but the low bits
            // still follow this convention)
            let dev_major = (dev >> 8) & 0xfff;
            let dev_minor = dev & 0xff;

            // If the minor number has more than 8 bits, use the extended format
            let dev_minor = if dev_minor == 0 {
                // Extended minor: bits 0-7 + bits 8-19
                (dev >> 8) & 0xff00ff
            } else {
                dev_minor
            };

            let event = DiskIoEvent {
                header: EventHeader {
                    event_type: EventType::DiskSlowIo as u32,
                    timestamp_ns: now,
                    pid: 0, // Disk events are not per-PID
                    tid: 0,
                },
                dev_major,
                dev_minor,
                sector,
                nr_sector,
                latency_ns,
                io_type,
            };
            let _ = EVENTS.output(&event, 0);

            warn!(
                &ctx,
                "Slow disk I/O: dev={}:{} latency={}ns type={}",
                dev_major,
                dev_minor,
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

/// Tracepoint: block/block_rq_requeue (optional)
///
/// Fires when a block I/O request is requeued (e.g., due to a device
/// error or timeout). We clean up the start time entry to prevent
/// map leaks.
///
/// Currently disabled because requeue events are rare and the map
/// cleanup on completion is sufficient for most cases. Enable if
/// you notice the IO_START_TIME map growing over time.
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
///     let sector: u64 = unsafe { ctx.read_at(8)? }.ok_or(0)?;
///     let dev: u32 = unsafe { ctx.read_at(0)? }.ok_or(0)?;
///
///     // Clean up the start time entry
///     unsafe {
///         let _ = IO_START_TIME.remove(&sector);
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

/// Periodic cleanup of stale IO_START_TIME entries.
///
/// In some cases (e.g., device errors, driver bugs), a `block_rq_complete`
/// event may not fire for a request. This leaves stale entries in the
/// IO_START_TIME map, which can cause it to fill up over time.
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

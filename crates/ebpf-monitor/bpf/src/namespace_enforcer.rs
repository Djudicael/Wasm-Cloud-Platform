//! eBPF namespace enforcement programs.
//!
//! These programs run in the kernel and provide:
//! 1. TCP connection tracking: detect when monitored TIDs connect to the gateway
//! 2. Audit: detect forged identity headers in send buffers
//!
//! # Maps
//!
//! - `MONITORED_TIDS`: HashMap<u32, TidIdentity> — populated by Supervisor at spawn time
//! - `NS_ENFORCE_CONFIG`: Array<NsEnforceConfig> — singleton configuration
//! - `EVENTS`: RingBuffer — events sent to userspace consumer

#![no_std]
#![no_main]

use aya_ebpf::{
    macros::{map, tracepoint},
    maps::{Array, HashMap, RingBuf},
    programs::TracePointContext,
    EbpfContext,
};
use ebpf_monitor_bpf::{
    EventHeader, EventType, NamespaceAuditEvent, NamespaceAuditType, NsEnforceConfig, TcpEvent,
    TidIdentity, IP_ADDR_LEN,
};

/// MONITORED_TIDS: u32 (TID) → TidIdentity
/// Populated by the Supervisor when an instance is spawned.
/// The key is the OS Thread ID (TID), not the PID.
#[map]
static MONITORED_TIDS: HashMap<u32, TidIdentity> = HashMap::with_max_entries(4096, 0);

/// NS_ENFORCE_CONFIG: singleton configuration array (index 0)
#[map]
static NS_ENFORCE_CONFIG: Array<NsEnforceConfig> = Array::with_max_entries(1, 0);

/// EVENTS: ring buffer for sending events to userspace
#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

// ── TCP State Constants ───────────────────────────────────────────────────────

const TCP_ESTABLISHED: u32 = 1;
const TCP_CLOSE: u32 = 7;

/// Tracepoint: inet_sock_set_state
///
/// Fires on every TCP state change. We monitor transitions to ESTABLISHED
/// and CLOSE for connections to the internal gateway port.
///
/// When a monitored TID connects to the gateway, we emit a TidConnection
/// event with the source port so the userspace consumer can update
/// the port_to_tid map.
#[tracepoint]
pub fn ns_inet_sock_set_state(ctx: TracePointContext) -> u32 {
    match try_ns_inet_sock_set_state(ctx) {
        Ok(()) => 0,
        Err(_) => 0, // eBPF programs must not panic
    }
}

fn try_ns_inet_sock_set_state(ctx: TracePointContext) -> Result<(), u32> {
    // Read tracepoint arguments from raw context.
    // The inet_sock_set_state tracepoint has these arguments:
    //   struct sock *sk, int oldstate, int newstate
    // But the raw tracepoint context gives us the raw fields.
    // We need to read from the tracepoint raw buffer.
    //
    // Layout (from kernel: include/trace/events/sock.h):
    //   __data_loc char name[32]   // comm
    //   const void *skaddr           // socket address
    //   u16 sport                    // source port
    //   u16 dport                    // destination port
    //   u16 family                   // AF_INET / AF_INET6
    //   __data_loc u8 *saddr         // source address
    //   __data_loc u8 *daddr         // destination address
    //   s32 oldstate                 // old TCP state
    //   s32 newstate                 // new TCP state

    let data = ctx.as_ptr() as *const u8;

    // Offsets for inet_sock_set_state tracepoint (verified on Linux 5.4+):
    // The exact layout depends on kernel version, but sport/dport are at
    // well-known offsets. We use conservative offset reading.
    //
    // On most kernels:
    //   offset 8:  skaddr (8 bytes)
    //   offset 16: sport (2 bytes)
    //   offset 18: dport (2 bytes)
    //   offset 20: family (2 bytes)
    //   offset 24: oldstate (4 bytes)
    //   offset 28: newstate (4 bytes)

    // Read ports
    let _old_state = unsafe { *(data.add(16) as *const i32) as u32 };
    let new_state = unsafe { *(data.add(20) as *const i32) as u32 };
    let sport = unsafe { *(data.add(24) as *const u16) };
    let dport = unsafe { *(data.add(26) as *const u16) };

    // Get current TID
    let tid = aya_ebpf::helpers::bpf_get_current_pid_tgid() as u32;

    // Look up config
    let config = match NS_ENFORCE_CONFIG.get(0) {
        Some(cfg) => cfg,
        None => return Ok(()),
    };

    let gateway_port = config.gateway_port;

    // Check if this is a connection to/from the gateway port
    let is_gateway_connection = sport == gateway_port || dport == gateway_port;

    if !is_gateway_connection {
        return Ok(());
    }

    // We only care about ESTABLISHED and CLOSE transitions
    if new_state == TCP_ESTABLISHED {
        // Connection established — check if the TID is monitored
        let is_monitored = unsafe { MONITORED_TIDS.get(&tid) }.is_some();

        if is_monitored {
            // Emit TidConnection event
            let mut event = match EVENTS.reserve::<TcpEvent>(0) {
                Some(e) => e,
                None => return Ok(()), // Ring buffer full
            };

            let connection = TcpEvent {
                header: EventHeader {
                    event_type: EventType::TidConnection as u32,
                    _padding: 0,
                    timestamp_ns: unsafe { aya_ebpf::helpers::gen::bpf_ktime_get_ns() },
                    pid: (aya_ebpf::helpers::bpf_get_current_pid_tgid() >> 32) as u32,
                    tid,
                },
                src_addr: [0; IP_ADDR_LEN],
                src_port: sport,
                dst_addr: [0; IP_ADDR_LEN],
                dst_port: dport,
                old_state: _old_state,
                new_state,
                retransmits: 0,
                rtt_us: 0,
                bytes: 0,
            };
            event.write(connection);
            event.submit(0);
        } else {
            // Unregistered TID connected to gateway — audit event
            let mut event = match EVENTS.reserve::<NamespaceAuditEvent>(0) {
                Some(e) => e,
                None => return Ok(()),
            };

            let header = EventHeader {
                event_type: EventType::NamespaceAudit as u32,
                _padding: 0,
                timestamp_ns: unsafe { aya_ebpf::helpers::gen::bpf_ktime_get_ns() },
                pid: (aya_ebpf::helpers::bpf_get_current_pid_tgid() >> 32) as u32,
                tid,
            };

            let audit = NamespaceAuditEvent {
                header,
                audit_type: NamespaceAuditType::UnregisteredTid as u32,
                source_namespace: [0u8; 64],
                source_app_id: [0u8; 64],
                dest_port: gateway_port,
                source_port: sport,
                _padding: 0,
                _tail_padding: 0,
            };

            event.write(audit);
            event.submit(0);
        }
    } else if new_state == TCP_CLOSE {
        // Connection closed — emit TidDisconnection
        let is_monitored = unsafe { MONITORED_TIDS.get(&tid) }.is_some();

        if is_monitored {
            let mut event = match EVENTS.reserve::<TcpEvent>(0) {
                Some(e) => e,
                None => return Ok(()),
            };

            let connection = TcpEvent {
                header: EventHeader {
                    event_type: EventType::TidDisconnection as u32,
                    _padding: 0,
                    timestamp_ns: unsafe { aya_ebpf::helpers::gen::bpf_ktime_get_ns() },
                    pid: (aya_ebpf::helpers::bpf_get_current_pid_tgid() >> 32) as u32,
                    tid,
                },
                src_addr: [0; IP_ADDR_LEN],
                src_port: sport,
                dst_addr: [0; IP_ADDR_LEN],
                dst_port: dport,
                old_state: _old_state,
                new_state,
                retransmits: 0,
                rtt_us: 0,
                bytes: 0,
            };
            event.write(connection);
            event.submit(0);
        }
    }

    Ok(())
}

/// Tracepoint: sys_enter_sendto / sys_enter_sendmsg
///
/// Audit send buffers for forged identity headers. When a monitored TID
/// sends data containing "X-Namespace:" or "X-Source-App:", emit a
/// security audit event.
///
/// Note: This is an audit-only program. eBPF tracepoints are read-only
/// and cannot modify userspace buffers. The gateway strips all identity
/// headers, so forged headers are harmless but logged.
#[tracepoint]
pub fn ns_audit_sendto(ctx: TracePointContext) -> u32 {
    match try_ns_audit_sendto(ctx) {
        Ok(()) => 0,
        Err(_) => 0,
    }
}

fn try_ns_audit_sendto(ctx: TracePointContext) -> Result<(), u32> {
    // Read sendto arguments from tracepoint context
    // sys_enter_sendto layout:
    //   int fd
    //   void *buf
    //   size_t len
    //   int flags
    //   struct sockaddr *addr
    //   int addr_len

    let data = ctx.as_ptr() as *const u8;

    // Offsets for sys_enter_sendto (x86_64):
    // These are approximate and may need adjustment per kernel version.
    let _fd = unsafe { *(data.add(16) as *const i32) };
    let buf_ptr = unsafe { *(data.add(24) as *const u64) };
    let len = unsafe { *(data.add(32) as *const u64) as usize };

    // Skip if no data
    if len == 0 || buf_ptr == 0 {
        return Ok(());
    }

    // Get current TID
    let tid = aya_ebpf::helpers::bpf_get_current_pid_tgid() as u32;

    // Check if this TID is monitored
    let identity = match unsafe { MONITORED_TIDS.get(&tid) } {
        Some(id) => id,
        None => return Ok(()),
    };

    // Check config for forged header detection enablement
    let config = match NS_ENFORCE_CONFIG.get(0) {
        Some(cfg) => cfg,
        None => return Ok(()),
    };

    let flags = config.flags;
    let detect_enabled = (flags & 2) != 0; // EnableForgedHeaderDetect = 2

    if !detect_enabled {
        return Ok(());
    }

    // Read buffer and scan for forged headers
    // We only scan the first 256 bytes (typical HTTP header size)
    let scan_len = len.min(256);
    let mut buf = [0u8; 16];

    // Patterns to detect (case-insensitive would be better but expensive in eBPF)
    let x_namespace: [u8; 12] = *b"X-Namespace:";
    let x_source_app: [u8; 13] = *b"X-Source-App:";

    // Simple scan: read chunks and check for patterns
    let mut found = false;
    let mut chunk_start = 0usize;

    while chunk_start + 16 <= scan_len && chunk_start <= 240 {
        if unsafe {
            aya_ebpf::helpers::gen::bpf_probe_read_user(
                buf.as_mut_ptr() as *mut _,
                16,
                (buf_ptr + chunk_start as u64) as *const _,
            )
        } < 0
        {
            break;
        }

        // Check four possible starting offsets, then advance by four bytes.
        // The fixed-size read gives Linux 6.1's verifier a non-negative,
        // statically bounded helper length while still covering every byte.
        for start in 0..4 {
            let mut match_ns = true;
            for i in 0..12 {
                if buf[start + i] != x_namespace[i] {
                    match_ns = false;
                    break;
                }
            }
            if match_ns {
                found = true;
                break;
            }

            let mut match_app = true;
            for i in 0..13 {
                if buf[start + i] != x_source_app[i] {
                    match_app = false;
                    break;
                }
            }
            if match_app {
                found = true;
                break;
            }
        }

        if found {
            break;
        }
        chunk_start += 4;
    }

    if found {
        // Emit forged header detection event
        let mut event = match EVENTS.reserve::<NamespaceAuditEvent>(0) {
            Some(e) => e,
            None => return Ok(()),
        };

        let header = EventHeader {
            event_type: EventType::NamespaceForgedHeader as u32,
            _padding: 0,
            timestamp_ns: unsafe { aya_ebpf::helpers::gen::bpf_ktime_get_ns() },
            pid: (aya_ebpf::helpers::bpf_get_current_pid_tgid() >> 32) as u32,
            tid,
        };

        let audit = NamespaceAuditEvent {
            header,
            audit_type: NamespaceAuditType::ForgedHeader as u32,
            source_namespace: identity.namespace,
            source_app_id: identity.app_id,
            dest_port: 0,
            source_port: 0,
            _padding: 0,
            _tail_padding: 0,
        };

        event.write(audit);
        event.submit(0);
    }

    Ok(())
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}

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
    maps::{Array, HashMap, LruHashMap, PerCpuArray, RingBuf},
    programs::TracePointContext,
    EbpfContext,
};
use ebpf_monitor_bpf::{
    EventHeader, EventType, GatewayOwnerIdentity, GatewayOwnerRef, GatewayPeerKey,
    NamespaceAuditEvent, NamespaceAuditType, NsEnforceConfig, TcpEvent, TidFlags, TidIdentity,
    IP_ADDR_LEN,
};

/// MONITORED_TIDS: u32 (TID) → TidIdentity
/// Populated by the Supervisor when an instance is spawned.
/// The key is the OS Thread ID (TID), not the PID.
#[map]
static MONITORED_TIDS: HashMap<u32, TidIdentity> = HashMap::with_max_entries(4096, 0);

/// Only sockets opened by registered workloads may remove a port binding.
/// CLOSE may run in another thread or kernel context, so retain socket ownership.
#[map]
static GATEWAY_SOCKETS: HashMap<u64, GatewayOwnerRef> = HashMap::with_max_entries(65536, 0);

/// Peer index is bounded and may retain an inactive owner reference. Only a
/// matching live GATEWAY_OWNERS record can authorize it.
#[map]
static GATEWAY_PEERS: LruHashMap<GatewayPeerKey, GatewayOwnerRef> =
    LruHashMap::with_max_entries(65536, 0);

#[map]
static GATEWAY_OWNERS: HashMap<GatewayOwnerRef, GatewayOwnerIdentity> =
    HashMap::with_max_entries(65536, 0);

/// NS_ENFORCE_CONFIG: singleton configuration array (index 0)
#[map]
static NS_ENFORCE_CONFIG: Array<NsEnforceConfig> = Array::with_max_entries(1, 0);

/// EVENTS: ring buffer for sending events to userspace
#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

#[map]
static DROPPED_EVENTS: PerCpuArray<u64> = PerCpuArray::with_max_entries(1, 0);

#[inline(always)]
fn record_drop() {
    if let Some(value) = DROPPED_EVENTS.get_ptr_mut(0) {
        unsafe { *value = (*value).saturating_add(1) };
    }
}

// ── TCP State Constants ───────────────────────────────────────────────────────

const TCP_SYN_SENT: u32 = 2;
const TCP_CLOSE: u32 = 7;

/// Match the gateway's fixed loopback listener, not arbitrary sockets using
/// the same destination port on another local address.
#[inline(always)]
unsafe fn gateway_peer(data: *const u8, source_port: u16) -> Option<GatewayPeerKey> {
    let family = *(data.add(28) as *const u16);
    let mut address = [0u8; 16];
    if family == 2 {
        let destination = *(data.add(36) as *const [u8; 4]);
        if destination != [127, 0, 0, 1] {
            return None;
        }
        let source = *(data.add(32) as *const [u8; 4]);
        address[..4].copy_from_slice(&source);
    } else if family == 10 {
        // Fixed-offset scalar reads avoid LLVM lowering an array slice copy
        // into a loop dereferencing a modified tracepoint context pointer.
        let d0 = *(data.add(56) as *const u32);
        let d1 = *(data.add(60) as *const u32);
        let d2 = *(data.add(64) as *const u32);
        let d3 = *(data.add(68) as *const u32);
        let is_loopback = d0 == 0 && d1 == 0 && d2 == 0 && d3 == u32::from_ne_bytes([0, 0, 0, 1]);
        let is_mapped = d0 == 0
            && d1 == 0
            && d2 == u32::from_ne_bytes([0, 0, 255, 255])
            && d3 == u32::from_ne_bytes([127, 0, 0, 1]);
        if !is_loopback && !is_mapped {
            return None;
        }
        let s0 = *(data.add(40) as *const u32);
        let s1 = *(data.add(44) as *const u32);
        let s2 = *(data.add(48) as *const u32);
        let s3 = *(data.add(52) as *const u32);
        let a = s0.to_ne_bytes();
        let b = s1.to_ne_bytes();
        let c = s2.to_ne_bytes();
        let d = s3.to_ne_bytes();
        if s0 == 0 && s1 == 0 && s2 == u32::from_ne_bytes([0, 0, 255, 255]) {
            address = [d[0], d[1], d[2], d[3], 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            return Some(GatewayPeerKey {
                address,
                source_port,
                family: 2,
            });
        }
        address = [
            a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3], c[0], c[1], c[2], c[3], d[0], d[1],
            d[2], d[3],
        ];
    } else {
        return None;
    }
    Some(GatewayPeerKey {
        address,
        source_port,
        family,
    })
}

/// Tracepoint: inet_sock_set_state
///
/// Fires on every TCP state change. We monitor transitions to SYN_SENT
/// and CLOSE for connections to the internal gateway port.
///
/// Publish live connection ownership directly before emitting TidConnection
/// telemetry. Gateway authorization does not depend on event delivery.
#[tracepoint]
pub fn ns_inet_sock_set_state(ctx: TracePointContext) -> u32 {
    match try_ns_inet_sock_set_state(ctx) {
        Ok(()) => 0,
        Err(_) => 0, // eBPF programs must not panic
    }
}

fn try_ns_inet_sock_set_state(ctx: TracePointContext) -> Result<(), u32> {
    // Raw tracepoint offsets include the 8-byte common header. Check the
    // target kernel's events/sock/inet_sock_set_state/format when porting.
    let data = ctx.as_ptr() as *const u8;
    let socket_key = unsafe { *(data.add(8) as *const u64) };
    let _old_state = unsafe { *(data.add(16) as *const i32) as u32 };
    let new_state = unsafe { *(data.add(20) as *const i32) as u32 };
    let sport = unsafe { *(data.add(24) as *const u16) };
    let dport = unsafe { *(data.add(26) as *const u16) };
    let protocol = unsafe { *(data.add(30) as *const u16) };
    if protocol != 6 {
        return Ok(());
    }

    // Get current TID
    let tid = ebpf_monitor_bpf::current_pid_tgid() as u32;

    // Look up config
    let config = match NS_ENFORCE_CONFIG.get(0) {
        Some(cfg) => cfg,
        None => return Ok(()),
    };

    let gateway_port = config.gateway_port;

    // Check if this is a connection to/from the gateway port
    // Only the client socket carries the caller's source port. The peer
    // socket's state changes must never create or remove that binding.
    let is_gateway_connection = dport == gateway_port;

    if !is_gateway_connection {
        return Ok(());
    }

    // SYN_SENT runs in connect() on the calling thread. ESTABLISHED can run
    // later in a softirq/kworker context and cannot identify the caller.
    if new_state == TCP_SYN_SENT {
        // A recycled socket address must not inherit an earlier registration.
        if let Some(previous) = unsafe { GATEWAY_SOCKETS.get(&socket_key).copied() } {
            let _ = GATEWAY_OWNERS.remove(&previous);
        }
        let _ = GATEWAY_SOCKETS.remove(&socket_key);
        let Some(peer) = (unsafe { gateway_peer(data, sport) }) else {
            return Ok(());
        };
        // Even an unregistered new connection invalidates an older peer index.
        let _ = GATEWAY_PEERS.remove(&peer);
        // Connection initiated — check if the TID is monitored
        let generation = unsafe { MONITORED_TIDS.get(&tid) }
            .filter(|identity| identity.flags & TidFlags::Enabled as u32 != 0)
            .map(|identity| identity.registered_at_ns);

        if let Some(registered_at_ns) = generation {
            let owner = GatewayOwnerRef {
                socket_id: socket_key,
                connected_at_ns: unsafe { aya_ebpf::helpers::gen::bpf_ktime_get_ns() },
            };
            let identity = GatewayOwnerIdentity {
                registered_at_ns,
                tid,
                _padding: 0,
            };
            if sport == 0
                || registered_at_ns == 0
                || GATEWAY_OWNERS.insert(&owner, &identity, 0).is_err()
            {
                record_drop();
                return Ok(());
            }
            if GATEWAY_SOCKETS.insert(&socket_key, &owner, 0).is_err()
                || GATEWAY_PEERS.insert(&peer, &owner, 0).is_err()
            {
                let _ = GATEWAY_OWNERS.remove(&owner);
                let _ = GATEWAY_SOCKETS.remove(&socket_key);
                record_drop();
                return Ok(());
            }
            // Authorization is already published. Lost/late telemetry cannot
            // prevent a gateway from resolving this connection's identity.
            // Emit TidConnection event
            let mut event = match EVENTS.reserve::<TcpEvent>(0) {
                Some(e) => e,
                None => {
                    record_drop();
                    return Ok(());
                }
            };

            let connection = TcpEvent {
                header: EventHeader {
                    event_type: EventType::TidConnection as u32,
                    _padding: 0,
                    timestamp_ns: unsafe { aya_ebpf::helpers::gen::bpf_ktime_get_ns() },
                    pid: (ebpf_monitor_bpf::current_pid_tgid() >> 32) as u32,
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
                None => {
                    record_drop();
                    return Ok(());
                }
            };

            let header = EventHeader {
                event_type: EventType::NamespaceAudit as u32,
                _padding: 0,
                timestamp_ns: unsafe { aya_ebpf::helpers::gen::bpf_ktime_get_ns() },
                pid: (ebpf_monitor_bpf::current_pid_tgid() >> 32) as u32,
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
        // Close can run outside the caller thread. Authenticate the socket,
        // not the current TID, and also remove failed-connect bindings.
        {
            let Some(owner) = (unsafe { GATEWAY_SOCKETS.get(&socket_key).copied() }) else {
                return Ok(());
            };
            let _ = GATEWAY_SOCKETS.remove(&socket_key);
            // Never delete the peer index here: a later connection may already
            // own it. An index without its exact live owner cannot authorize.
            let _ = GATEWAY_OWNERS.remove(&owner);
            let mut event = match EVENTS.reserve::<TcpEvent>(0) {
                Some(e) => e,
                None => {
                    record_drop();
                    return Ok(());
                }
            };

            let connection = TcpEvent {
                header: EventHeader {
                    event_type: EventType::TidDisconnection as u32,
                    _padding: 0,
                    timestamp_ns: unsafe { aya_ebpf::helpers::gen::bpf_ktime_get_ns() },
                    pid: (ebpf_monitor_bpf::current_pid_tgid() >> 32) as u32,
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
    let tid = ebpf_monitor_bpf::current_pid_tgid() as u32;

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
            None => {
                record_drop();
                return Ok(());
            }
        };

        let header = EventHeader {
            event_type: EventType::NamespaceForgedHeader as u32,
            _padding: 0,
            timestamp_ns: unsafe { aya_ebpf::helpers::gen::bpf_ktime_get_ns() },
            pid: (ebpf_monitor_bpf::current_pid_tgid() >> 32) as u32,
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

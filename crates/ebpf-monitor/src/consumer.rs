//! Ring buffer consumer for eBPF events.
//!
//! Reads raw bytes from the eBPF ring buffer (when the `ebpf` feature is active),
//! parses them into typed `MonitorEvent` structs, and dispatches them to the
//! `ActionDispatcher` for metric updates and recovery actions.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────┐    mpsc channel    ┌──────────────────┐
//! │  Ring Buf   │ ───────────────▶  │ Action Dispatcher │
//! │  Consumer   │  MonitorEvent    │ (metrics + actions)│
//! └─────────────┘                   └──────────────────┘
//! ```
//!
//! The ring buffer consumer runs in its own Tokio task and sends parsed events
//! through an mpsc channel. This decouples the fast ring-buffer polling loop
//! from the potentially slower action dispatch (which may involve IPC, process
//! kills, or NATS publishes).

use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info};

use crate::actions::{ActionDispatcher, MonitorEvent};
use crate::common::{
    DiskIoEvent, EventHeader, EventType, FdEvent, MemPressureEvent, NamespaceAuditEvent,
    NamespaceAuditType, ProcessEvent, SyscallCategory, SyscallEvent, TcpEvent,
};

// ── Event Parsing (no aya dependency) ──────────────────────────────────────────

/// Error type for event parsing failures.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("buffer too small: got {got} bytes, need at least {need}")]
    BufferTooSmall { got: usize, need: usize },

    #[error("unknown event type: {0}")]
    UnknownEventType(u32),

    #[error("event size mismatch: type {event_type} expected {expected} bytes, got {actual}")]
    SizeMismatch {
        event_type: u32,
        expected: usize,
        actual: usize,
    },
}

/// Parse a raw byte slice from the ring buffer into a `MonitorEvent`.
///
/// The byte layout is: `[EventHeader][event-specific payload]`.
/// All structs are `#[repr(C)]` with fixed sizes, so we can read them
/// directly from the byte slice.
pub fn parse_event(bytes: &[u8]) -> Result<MonitorEvent, ParseError> {
    let header_size = std::mem::size_of::<EventHeader>();
    if bytes.len() < header_size {
        return Err(ParseError::BufferTooSmall {
            got: bytes.len(),
            need: header_size,
        });
    }

    let header = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const EventHeader) };
    let event_type = EventType::from_u32(header.event_type)
        .ok_or(ParseError::UnknownEventType(header.event_type))?;

    match event_type {
        EventType::ProcessExec | EventType::ProcessExit => {
            let expected = std::mem::size_of::<ProcessEvent>();
            if bytes.len() < expected {
                return Err(ParseError::SizeMismatch {
                    event_type: header.event_type,
                    expected,
                    actual: bytes.len(),
                });
            }
            let event = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const ProcessEvent) };
            if event_type == EventType::ProcessExec {
                Ok(MonitorEvent::ProcessExec {
                    pid: event.header.pid,
                    ppid: event.ppid,
                    comm: event.comm,
                    cgroup_id: event.cgroup_id,
                })
            } else {
                Ok(MonitorEvent::ProcessExit {
                    pid: event.header.pid,
                    ppid: event.ppid,
                    exit_code: event.exit_code,
                    signal: event.signal,
                    comm: event.comm,
                    cgroup_id: event.cgroup_id,
                })
            }
        }

        EventType::TcpConnect | EventType::TcpClose | EventType::TcpRetransmit => {
            let expected = std::mem::size_of::<TcpEvent>();
            if bytes.len() < expected {
                return Err(ParseError::SizeMismatch {
                    event_type: header.event_type,
                    expected,
                    actual: bytes.len(),
                });
            }
            let event = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const TcpEvent) };
            match event_type {
                EventType::TcpConnect => Ok(MonitorEvent::TcpConnect {
                    pid: event.header.pid,
                    src_port: event.src_port,
                    dst_port: event.dst_port,
                    old_state: event.old_state,
                    new_state: event.new_state,
                }),
                EventType::TcpClose => Ok(MonitorEvent::TcpClose {
                    pid: event.header.pid,
                    src_port: event.src_port,
                    dst_port: event.dst_port,
                }),
                EventType::TcpRetransmit => Ok(MonitorEvent::TcpRetransmit {
                    pid: event.header.pid,
                    src_port: event.src_port,
                    dst_port: event.dst_port,
                    retransmits: event.retransmits,
                    rtt_us: event.rtt_us,
                }),
                _ => unreachable!(),
            }
        }

        EventType::FdOpen | EventType::FdLimitApproaching => {
            let expected = std::mem::size_of::<FdEvent>();
            if bytes.len() < expected {
                return Err(ParseError::SizeMismatch {
                    event_type: header.event_type,
                    expected,
                    actual: bytes.len(),
                });
            }
            let event = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const FdEvent) };
            if event_type == EventType::FdOpen {
                Ok(MonitorEvent::FdOpen {
                    pid: event.header.pid,
                    fd: event.fd,
                    current_fd_count: event.current_fd_count,
                    fd_soft_limit: event.fd_soft_limit,
                })
            } else {
                Ok(MonitorEvent::FdLimitApproaching {
                    pid: event.header.pid,
                    fd: event.fd,
                    current_fd_count: event.current_fd_count,
                    fd_soft_limit: event.fd_soft_limit,
                })
            }
        }

        EventType::MemPressure => {
            let expected = std::mem::size_of::<MemPressureEvent>();
            if bytes.len() < expected {
                return Err(ParseError::SizeMismatch {
                    event_type: header.event_type,
                    expected,
                    actual: bytes.len(),
                });
            }
            let event =
                unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const MemPressureEvent) };
            Ok(MonitorEvent::MemPressure {
                pid: event.header.pid,
                free_pages: event.free_pages,
                reclaim_pages: event.reclaim_pages,
                pressure_level: event.pressure_level,
                anon_pages: event.anon_pages,
            })
        }

        EventType::DiskSlowIo => {
            let expected = std::mem::size_of::<DiskIoEvent>();
            if bytes.len() < expected {
                return Err(ParseError::SizeMismatch {
                    event_type: header.event_type,
                    expected,
                    actual: bytes.len(),
                });
            }
            let event = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const DiskIoEvent) };
            Ok(MonitorEvent::DiskSlowIo {
                dev_major: event.dev_major,
                dev_minor: event.dev_minor,
                latency_ns: event.latency_ns,
                io_type: event.io_type,
            })
        }

        EventType::SyscallAnomaly => {
            let expected = std::mem::size_of::<SyscallEvent>();
            if bytes.len() < expected {
                return Err(ParseError::SizeMismatch {
                    event_type: header.event_type,
                    expected,
                    actual: bytes.len(),
                });
            }
            let event = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const SyscallEvent) };
            Ok(MonitorEvent::SyscallAnomaly {
                pid: event.header.pid,
                syscall_nr: event.syscall_nr,
                syscall_category: SyscallCategory::from_u32(event.syscall_category),
                count_in_window: event.count_in_window,
            })
        }

        EventType::TidConnection | EventType::TidDisconnection => {
            let expected = std::mem::size_of::<TcpEvent>();
            if bytes.len() < expected {
                return Err(ParseError::SizeMismatch {
                    event_type: header.event_type,
                    expected,
                    actual: bytes.len(),
                });
            }
            let event = unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const TcpEvent) };
            if event_type == EventType::TidConnection {
                // Look up the TID identity from the MONITORED_TIDS map context
                // (the eBPF program already validated the TID is registered)
                Ok(MonitorEvent::TidConnection {
                    tid: event.header.tid,
                    namespace: String::new(), // Will be filled by consumer loop
                    app_id: String::new(),    // Will be filled by consumer loop
                    source_port: event.src_port,
                })
            } else {
                Ok(MonitorEvent::TidDisconnection {
                    tid: event.header.tid,
                    source_port: event.src_port,
                })
            }
        }

        EventType::NamespaceAudit | EventType::NamespaceForgedHeader => {
            let expected = std::mem::size_of::<NamespaceAuditEvent>();
            if bytes.len() < expected {
                return Err(ParseError::SizeMismatch {
                    event_type: header.event_type,
                    expected,
                    actual: bytes.len(),
                });
            }
            let event =
                unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const NamespaceAuditEvent) };
            let ns = read_cstr(&event.source_namespace);
            let app = read_cstr(&event.source_app_id);
            match NamespaceAuditType::from_u32(event.audit_type) {
                Some(NamespaceAuditType::ForgedHeader) | None
                    if event_type == EventType::NamespaceForgedHeader =>
                {
                    Ok(MonitorEvent::NamespaceForgedHeader {
                        tid: event.header.tid,
                        namespace: ns,
                        app_id: app,
                    })
                }
                Some(NamespaceAuditType::UnregisteredTid) => {
                    Ok(MonitorEvent::UnregisteredTidConnection {
                        tid: event.header.tid,
                    })
                }
                _ => Ok(MonitorEvent::NamespaceAudit {
                    tid: event.header.tid,
                    namespace: ns,
                    app_id: app,
                }),
            }
        }
    }
}

/// Read a null-terminated C string from a fixed-size byte array.
fn read_cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// ── Ring Buffer Consumer (ebpf feature) ──────────────────────────────────────

#[cfg(feature = "ebpf")]
mod ebpf_consumer {
    use super::*;
    use aya::maps::RingBuf as AyaRingBuf;

    /// Read events from the eBPF ring buffer and send them to the action dispatcher.
    ///
    /// This function runs in a tight loop, polling the ring buffer every `poll_interval`.
    /// Parsed events are sent through the `action_tx` channel. Malformed events are
    /// logged and skipped (no panic).
    ///
    /// The function returns when the `action_tx` channel is closed (i.e., the receiver
    /// was dropped), which signals a clean shutdown.
    pub async fn consume_ring_buffer(
        mut ring_buf: AyaRingBuf<AyaRingBuf>,
        action_tx: tokio::sync::mpsc::Sender<MonitorEvent>,
        metrics: Arc<EbpfMetrics>,
        poll_interval: Duration,
    ) {
        info!(
            interval_ms = poll_interval.as_millis(),
            "eBPF ring buffer consumer started"
        );

        let mut interval = tokio::time::interval(poll_interval);
        let mut consecutive_errors = 0u32;

        loop {
            interval.tick().await;

            let mut events_this_tick = 0u32;

            // Drain all available events from the ring buffer
            while let Some(item) = ring_buf.next() {
                let raw_bytes = item.as_ref();

                match parse_event(raw_bytes) {
                    Ok(event) => {
                        events_this_tick += 1;
                        if action_tx.send(event).await.is_err() {
                            info!("action channel closed — eBPF consumer shutting down");
                            return;
                        }
                    }
                    Err(ParseError::UnknownEventType(t)) => {
                        // Unknown event types are expected if the eBPF program is newer
                        // than the userspace code. Log at debug level.
                        debug!(event_type = t, "skipping unknown eBPF event type");
                        metrics.events_parse_errors.inc();
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to parse eBPF event");
                        metrics.events_parse_errors.inc();
                        consecutive_errors += 1;

                        // If we get too many consecutive parse errors, something is
                        // fundamentally wrong with the ring buffer data. Slow down.
                        if consecutive_errors > 100 {
                            error!(
                                consecutive_errors,
                                "too many consecutive parse errors — possible data corruption"
                            );
                            // Back off for a second before continuing
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            consecutive_errors = 0;
                        }
                    }
                }
            }

            // Reset error counter on successful ticks
            if events_this_tick > 0 {
                consecutive_errors = 0;
            }

            if events_this_tick > 0 {
                debug!(events = events_this_tick, "eBPF events processed this tick");
            }
        }
    }
}

#[cfg(feature = "ebpf")]
pub use ebpf_consumer::consume_ring_buffer;

// ── Action Dispatcher Loop ────────────────────────────────────────────────────

/// Run the action dispatcher loop, receiving events from the mpsc channel
/// and dispatching them to the `ActionDispatcher`.
///
/// This function runs until the sender is dropped (channel closed).
pub async fn run_action_dispatcher(
    mut event_rx: tokio::sync::mpsc::Receiver<MonitorEvent>,
    dispatcher: Arc<ActionDispatcher>,
) {
    info!("action dispatcher loop started");

    while let Some(event) = event_rx.recv().await {
        dispatcher.dispatch(event);
    }

    info!("action dispatcher loop stopped (channel closed)");
}

// ── Consumer Start Helper ─────────────────────────────────────────────────────

/// Configuration for the consumer startup.
pub struct ConsumerConfig {
    /// Polling interval for the ring buffer (default: 10ms).
    pub poll_interval: Duration,
    /// Channel capacity for the event dispatch queue.
    pub channel_capacity: usize,
}

impl Default for ConsumerConfig {
    fn default() -> Self {
        ConsumerConfig {
            poll_interval: Duration::from_millis(10),
            channel_capacity: 4096,
        }
    }
}

/// Start the action dispatcher as a background task.
///
/// Returns the sender half of the mpsc channel. The caller should send
/// `MonitorEvent`s through this sender. When the sender is dropped, the
/// dispatcher loop will exit cleanly.
pub fn start_action_dispatcher(
    dispatcher: Arc<ActionDispatcher>,
    config: ConsumerConfig,
) -> tokio::sync::mpsc::Sender<MonitorEvent> {
    let (action_tx, action_rx) = tokio::sync::mpsc::channel(config.channel_capacity);

    tokio::spawn(async move {
        run_action_dispatcher(action_rx, dispatcher).await;
    });

    action_tx
}

// ── Unit Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{EventType, SyscallCategory, TASK_COMM_LEN};

    /// Helper to serialize a `#[repr(C)]` struct into a byte vector.
    fn struct_to_bytes<T>(val: &T) -> Vec<u8> {
        let size = std::mem::size_of::<T>();
        let mut bytes = vec![0u8; size];
        unsafe {
            std::ptr::copy_nonoverlapping(val as *const T as *const u8, bytes.as_mut_ptr(), size);
        }
        bytes
    }

    #[test]
    fn test_parse_process_exec_event() {
        let event = ProcessEvent {
            header: EventHeader {
                event_type: EventType::ProcessExec as u32,
                timestamp_ns: 1000000,
                pid: 42,
                tid: 43,
            },
            comm: [b't'; TASK_COMM_LEN],
            exit_code: 0,
            signal: 0,
            ppid: 1,
            cgroup_id: 12345,
        };

        let bytes = struct_to_bytes(&event);
        let parsed = parse_event(&bytes).unwrap();

        match parsed {
            MonitorEvent::ProcessExec {
                pid,
                ppid,
                comm,
                cgroup_id,
            } => {
                assert_eq!(pid, 42);
                assert_eq!(ppid, 1);
                assert_eq!(comm, [b't'; TASK_COMM_LEN]);
                assert_eq!(cgroup_id, 12345);
            }
            _ => panic!("expected ProcessExec, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_process_exit_event() {
        let event = ProcessEvent {
            header: EventHeader {
                event_type: EventType::ProcessExit as u32,
                timestamp_ns: 2000000,
                pid: 99,
                tid: 99,
            },
            comm: [0u8; TASK_COMM_LEN],
            exit_code: 1,
            signal: 9, // OOM kill
            ppid: 1,
            cgroup_id: 0,
        };

        let bytes = struct_to_bytes(&event);
        let parsed = parse_event(&bytes).unwrap();

        match parsed {
            MonitorEvent::ProcessExit {
                pid,
                exit_code,
                signal,
                ..
            } => {
                assert_eq!(pid, 99);
                assert_eq!(exit_code, 1);
                assert_eq!(signal, 9);
            }
            _ => panic!("expected ProcessExit, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_tcp_connect_event() {
        let event = TcpEvent {
            header: EventHeader {
                event_type: EventType::TcpConnect as u32,
                timestamp_ns: 3000000,
                pid: 100,
                tid: 100,
            },
            src_addr: [0u8; 16],
            src_port: 4222,
            dst_addr: [0u8; 16],
            dst_port: 54321,
            old_state: 0,
            new_state: 1,
            retransmits: 0,
            rtt_us: 500,
        };

        let bytes = struct_to_bytes(&event);
        let parsed = parse_event(&bytes).unwrap();

        match parsed {
            MonitorEvent::TcpConnect {
                pid,
                src_port,
                dst_port,
                ..
            } => {
                assert_eq!(pid, 100);
                assert_eq!(src_port, 4222);
                assert_eq!(dst_port, 54321);
            }
            _ => panic!("expected TcpConnect, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_tcp_retransmit_event() {
        let event = TcpEvent {
            header: EventHeader {
                event_type: EventType::TcpRetransmit as u32,
                timestamp_ns: 4000000,
                pid: 200,
                tid: 200,
            },
            src_addr: [0u8; 16],
            src_port: 4222,
            dst_addr: [0u8; 16],
            dst_port: 12345,
            old_state: 1,
            new_state: 5,
            retransmits: 10,
            rtt_us: 2000,
        };

        let bytes = struct_to_bytes(&event);
        let parsed = parse_event(&bytes).unwrap();

        match parsed {
            MonitorEvent::TcpRetransmit {
                pid,
                src_port,
                retransmits,
                rtt_us,
                ..
            } => {
                assert_eq!(pid, 200);
                assert_eq!(src_port, 4222);
                assert_eq!(retransmits, 10);
                assert_eq!(rtt_us, 2000);
            }
            _ => panic!("expected TcpRetransmit, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_fd_limit_approaching_event() {
        let event = FdEvent {
            header: EventHeader {
                event_type: EventType::FdLimitApproaching as u32,
                timestamp_ns: 5000000,
                pid: 300,
                tid: 300,
            },
            fd: 8000,
            fd_type: 0,
            current_fd_count: 8000,
            fd_soft_limit: 8192,
        };

        let bytes = struct_to_bytes(&event);
        let parsed = parse_event(&bytes).unwrap();

        match parsed {
            MonitorEvent::FdLimitApproaching {
                pid,
                current_fd_count,
                fd_soft_limit,
                ..
            } => {
                assert_eq!(pid, 300);
                assert_eq!(current_fd_count, 8000);
                assert_eq!(fd_soft_limit, 8192);
            }
            _ => panic!("expected FdLimitApproaching, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_mem_pressure_event() {
        let event = MemPressureEvent {
            header: EventHeader {
                event_type: EventType::MemPressure as u32,
                timestamp_ns: 6000000,
                pid: 400,
                tid: 400,
            },
            free_pages: 50000,
            reclaim_pages: 1000,
            pressure_level: 2, // critical
            anon_pages: 30000,
        };

        let bytes = struct_to_bytes(&event);
        let parsed = parse_event(&bytes).unwrap();

        match parsed {
            MonitorEvent::MemPressure {
                free_pages,
                pressure_level,
                ..
            } => {
                assert_eq!(free_pages, 50000);
                assert_eq!(pressure_level, 2);
            }
            _ => panic!("expected MemPressure, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_disk_slow_io_event() {
        let event = DiskIoEvent {
            header: EventHeader {
                event_type: EventType::DiskSlowIo as u32,
                timestamp_ns: 7000000,
                pid: 0,
                tid: 0,
            },
            dev_major: 8,
            dev_minor: 0,
            sector: 123456,
            nr_sector: 8,
            latency_ns: 100_000_000, // 100ms
            io_type: 1,              // write
        };

        let bytes = struct_to_bytes(&event);
        let parsed = parse_event(&bytes).unwrap();

        match parsed {
            MonitorEvent::DiskSlowIo {
                dev_major,
                dev_minor,
                latency_ns,
                io_type,
            } => {
                assert_eq!(dev_major, 8);
                assert_eq!(dev_minor, 0);
                assert_eq!(latency_ns, 100_000_000);
                assert_eq!(io_type, 1);
            }
            _ => panic!("expected DiskSlowIo, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_syscall_anomaly_event() {
        let event = SyscallEvent {
            header: EventHeader {
                event_type: EventType::SyscallAnomaly as u32,
                timestamp_ns: 8000000,
                pid: 500,
                tid: 500,
            },
            syscall_nr: 101, // SYS_PTRACE
            syscall_category: SyscallCategory::PrivilegeEscalation as u32,
            count_in_window: 1,
        };

        let bytes = struct_to_bytes(&event);
        let parsed = parse_event(&bytes).unwrap();

        match parsed {
            MonitorEvent::SyscallAnomaly {
                pid,
                syscall_nr,
                syscall_category,
                count_in_window,
            } => {
                assert_eq!(pid, 500);
                assert_eq!(syscall_nr, 101);
                assert_eq!(syscall_category, SyscallCategory::PrivilegeEscalation);
                assert_eq!(count_in_window, 1);
            }
            _ => panic!("expected SyscallAnomaly, got {:?}", parsed),
        }
    }

    #[test]
    fn test_parse_buffer_too_small() {
        let bytes = [0u8; 4]; // Way too small for any event
        let result = parse_event(&bytes);
        assert!(matches!(result, Err(ParseError::BufferTooSmall { .. })));
    }

    #[test]
    fn test_parse_unknown_event_type() {
        // Create a valid header with an unknown event type
        let header = EventHeader {
            event_type: 255, // Unknown
            timestamp_ns: 0,
            pid: 0,
            tid: 0,
        };
        let bytes = struct_to_bytes(&header);
        let result = parse_event(&bytes);
        assert!(matches!(result, Err(ParseError::UnknownEventType(255))));
    }

    #[test]
    fn test_parse_size_mismatch() {
        // Create a header for ProcessExec but with insufficient bytes for the full struct
        let header = EventHeader {
            event_type: EventType::ProcessExec as u32,
            timestamp_ns: 0,
            pid: 0,
            tid: 0,
        };
        let mut bytes = struct_to_bytes(&header);
        // Add a few extra bytes but not enough for a full ProcessEvent
        bytes.extend_from_slice(&[0u8; 4]);

        let result = parse_event(&bytes);
        assert!(matches!(result, Err(ParseError::SizeMismatch { .. })));
    }

    #[test]
    fn test_parse_empty_buffer() {
        let result = parse_event(&[]);
        assert!(matches!(result, Err(ParseError::BufferTooSmall { .. })));
    }

    #[tokio::test]
    async fn test_action_dispatcher_loop() {
        use prometheus::Registry;

        let registry = Registry::new();
        let metrics = Arc::new(crate::EbpfMetrics::new(&registry));
        let dispatcher = Arc::new(ActionDispatcher::new_noop(
            metrics.clone(),
            "test".to_string(),
        ));

        let (tx, rx) = tokio::sync::mpsc::channel::<MonitorEvent>(100);

        // Spawn the dispatcher loop
        let disp = dispatcher.clone();
        let handle: tokio::task::JoinHandle<()> = tokio::spawn(async move {
            run_action_dispatcher(rx, disp).await;
        });

        // Send some events
        tx.send(MonitorEvent::ProcessExec {
            pid: 1,
            ppid: 0,
            comm: [0; 16],
            cgroup_id: 0,
        })
        .await
        .unwrap();

        tx.send(MonitorEvent::MemPressure {
            pid: 0,
            free_pages: 50000,
            reclaim_pages: 0,
            pressure_level: 1,
            anon_pages: 10000,
        })
        .await
        .unwrap();

        // Drop the sender to close the channel
        drop(tx);

        // The dispatcher loop should exit cleanly
        let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(result.is_ok());

        // Verify metrics were updated
        assert_eq!(metrics.events_processed.get(), 2);
        assert_eq!(metrics.memory_pressure_level.get(), 1);
    }

    #[tokio::test]
    async fn test_start_action_dispatcher() {
        use prometheus::Registry;
        use std::time::Duration;

        let registry = Registry::new();
        let metrics = Arc::new(crate::EbpfMetrics::new(&registry));
        let dispatcher = Arc::new(ActionDispatcher::new_noop(
            metrics.clone(),
            "test".to_string(),
        ));

        let tx = start_action_dispatcher(dispatcher, ConsumerConfig::default());

        // Send an event through the channel
        tx.send(MonitorEvent::DiskSlowIo {
            dev_major: 8,
            dev_minor: 0,
            latency_ns: 50_000_000,
            io_type: 0,
        })
        .await
        .unwrap();

        // Give the dispatcher time to process
        tokio::time::sleep(Duration::from_millis(100)).await;

        assert_eq!(metrics.events_processed.get(), 1);
        assert_eq!(metrics.disk_io_latency_seconds.get_sample_count(), 1);
    }

    #[test]
    fn test_consumer_config_default() {
        let config = ConsumerConfig::default();
        assert_eq!(config.poll_interval, Duration::from_millis(10));
        assert_eq!(config.channel_capacity, 4096);
    }

    #[test]
    fn test_parse_all_event_types_roundtrip() {
        // Verify that all event types can be parsed from their raw struct representation
        let test_cases: Vec<(EventType, Vec<u8>)> = vec![
            (
                EventType::ProcessExec,
                struct_to_bytes(&ProcessEvent {
                    header: EventHeader {
                        event_type: EventType::ProcessExec as u32,
                        timestamp_ns: 1,
                        pid: 1,
                        tid: 1,
                    },
                    comm: [0; TASK_COMM_LEN],
                    exit_code: 0,
                    signal: 0,
                    ppid: 0,
                    cgroup_id: 0,
                }),
            ),
            (
                EventType::ProcessExit,
                struct_to_bytes(&ProcessEvent {
                    header: EventHeader {
                        event_type: EventType::ProcessExit as u32,
                        timestamp_ns: 2,
                        pid: 2,
                        tid: 2,
                    },
                    comm: [0; TASK_COMM_LEN],
                    exit_code: 0,
                    signal: 9,
                    ppid: 1,
                    cgroup_id: 0,
                }),
            ),
            (
                EventType::TcpConnect,
                struct_to_bytes(&TcpEvent {
                    header: EventHeader {
                        event_type: EventType::TcpConnect as u32,
                        timestamp_ns: 3,
                        pid: 3,
                        tid: 3,
                    },
                    src_addr: [0; 16],
                    src_port: 80,
                    dst_addr: [0; 16],
                    dst_port: 443,
                    old_state: 0,
                    new_state: 1,
                    retransmits: 0,
                    rtt_us: 0,
                }),
            ),
            (
                EventType::TcpClose,
                struct_to_bytes(&TcpEvent {
                    header: EventHeader {
                        event_type: EventType::TcpClose as u32,
                        timestamp_ns: 4,
                        pid: 4,
                        tid: 4,
                    },
                    src_addr: [0; 16],
                    src_port: 80,
                    dst_addr: [0; 16],
                    dst_port: 443,
                    old_state: 1,
                    new_state: 7,
                    retransmits: 0,
                    rtt_us: 0,
                }),
            ),
            (
                EventType::TcpRetransmit,
                struct_to_bytes(&TcpEvent {
                    header: EventHeader {
                        event_type: EventType::TcpRetransmit as u32,
                        timestamp_ns: 5,
                        pid: 5,
                        tid: 5,
                    },
                    src_addr: [0; 16],
                    src_port: 4222,
                    dst_addr: [0; 16],
                    dst_port: 12345,
                    old_state: 1,
                    new_state: 5,
                    retransmits: 3,
                    rtt_us: 1000,
                }),
            ),
            (
                EventType::FdOpen,
                struct_to_bytes(&FdEvent {
                    header: EventHeader {
                        event_type: EventType::FdOpen as u32,
                        timestamp_ns: 6,
                        pid: 6,
                        tid: 6,
                    },
                    fd: 10,
                    fd_type: 0,
                    current_fd_count: 100,
                    fd_soft_limit: 8192,
                }),
            ),
            (
                EventType::FdLimitApproaching,
                struct_to_bytes(&FdEvent {
                    header: EventHeader {
                        event_type: EventType::FdLimitApproaching as u32,
                        timestamp_ns: 7,
                        pid: 7,
                        tid: 7,
                    },
                    fd: 8000,
                    fd_type: 0,
                    current_fd_count: 8000,
                    fd_soft_limit: 8192,
                }),
            ),
            (
                EventType::MemPressure,
                struct_to_bytes(&MemPressureEvent {
                    header: EventHeader {
                        event_type: EventType::MemPressure as u32,
                        timestamp_ns: 8,
                        pid: 8,
                        tid: 8,
                    },
                    free_pages: 50000,
                    reclaim_pages: 1000,
                    pressure_level: 2,
                    anon_pages: 30000,
                }),
            ),
            (
                EventType::DiskSlowIo,
                struct_to_bytes(&DiskIoEvent {
                    header: EventHeader {
                        event_type: EventType::DiskSlowIo as u32,
                        timestamp_ns: 9,
                        pid: 0,
                        tid: 0,
                    },
                    dev_major: 8,
                    dev_minor: 0,
                    sector: 1234,
                    nr_sector: 8,
                    latency_ns: 50_000_000,
                    io_type: 1,
                }),
            ),
            (
                EventType::SyscallAnomaly,
                struct_to_bytes(&SyscallEvent {
                    header: EventHeader {
                        event_type: EventType::SyscallAnomaly as u32,
                        timestamp_ns: 10,
                        pid: 10,
                        tid: 10,
                    },
                    syscall_nr: 101,
                    syscall_category: SyscallCategory::PrivilegeEscalation as u32,
                    count_in_window: 1,
                }),
            ),
        ];

        for (expected_type, bytes) in test_cases {
            let parsed = parse_event(&bytes);
            assert!(
                parsed.is_ok(),
                "failed to parse {:?}: {:?}",
                expected_type,
                parsed.err()
            );
            assert_eq!(parsed.unwrap().event_type(), expected_type);
        }
    }
}

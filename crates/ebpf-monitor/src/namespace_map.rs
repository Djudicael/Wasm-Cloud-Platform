//! Userspace API for the MONITORED_TIDS eBPF map.
//!
//! Provides `register_tid` and `deregister_tid` operations that the
//! Supervisor calls when spawning/killing instances.
//! Also provides the `resolve_identity` method that the gateway calls
//! to answer "who is calling from source port X?"

#[cfg(feature = "ebpf")]
use crate::common::GatewayPeerKey;
use crate::common::{GatewayOwnerIdentity, GatewayOwnerRef, TidFlags, TidIdentity};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "ebpf")]
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// One active application identity registered for eBPF filtering.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MonitoredTidStatus {
    pub tid: u32,
    pub namespace: String,
    pub app_id: String,
    pub registered_at_ns: u64,
    /// Linux scheduler counters for the dedicated application executor thread.
    pub performance: Option<TaskPerformanceStatus>,
}

/// Cumulative Linux scheduler counters for one thread.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskPerformanceStatus {
    pub user_cpu_ticks: u64,
    pub system_cpu_ticks: u64,
    pub voluntary_context_switches: u64,
    pub nonvoluntary_context_switches: u64,
}

/// Cumulative Linux resource counters for the node process.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProcessPerformanceStatus {
    pub pid: u32,
    pub user_cpu_ticks: u64,
    pub system_cpu_ticks: u64,
    pub resident_memory_bytes: u64,
    pub voluntary_context_switches: u64,
    pub nonvoluntary_context_switches: u64,
}

/// Bounded lifecycle snapshot for the authenticated admin API.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NamespaceMapStatus {
    pub active_tids: Vec<MonitoredTidStatus>,
    /// Number of scheduler ticks per second used by the CPU tick fields.
    pub clock_ticks_per_second: u64,
    /// Resource counters for the complete `wasm-node` process.
    pub process_performance: Option<ProcessPerformanceStatus>,
    pub recent_tombstones: usize,
    pub port_bindings: usize,
    pub kernel_map_entry_counts: Vec<usize>,
    pub bpffs_mounted: bool,
    pub pinned_entries: usize,
}

fn parse_proc_stat(stat: &str) -> Option<(u64, u64, i64)> {
    // The command name is parenthesized and may contain spaces or parentheses.
    // Fields after its final ')' start at Linux procfs field 3 (`state`).
    let fields: Vec<_> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    Some((
        fields.get(11)?.parse().ok()?,
        fields.get(12)?.parse().ok()?,
        fields.get(21)?.parse().ok()?,
    ))
}

fn parse_context_switches(status: &str) -> Option<(u64, u64)> {
    let value = |prefix: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(prefix))?
            .trim()
            .parse()
            .ok()
    };
    Some((
        value("voluntary_ctxt_switches:")?,
        value("nonvoluntary_ctxt_switches:")?,
    ))
}

#[cfg(target_os = "linux")]
fn process_context_switches() -> Option<(u64, u64)> {
    let mut voluntary = 0_u64;
    let mut nonvoluntary = 0_u64;
    let mut observed = false;
    for entry in std::fs::read_dir("/proc/self/task")
        .ok()?
        .filter_map(Result::ok)
    {
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let Some((task_voluntary, task_nonvoluntary)) = parse_context_switches(&status) else {
            continue;
        };
        voluntary = voluntary.saturating_add(task_voluntary);
        nonvoluntary = nonvoluntary.saturating_add(task_nonvoluntary);
        observed = true;
    }
    observed.then_some((voluntary, nonvoluntary))
}

#[cfg(target_os = "linux")]
fn task_performance(tid: u32) -> Option<TaskPerformanceStatus> {
    let root = format!("/proc/self/task/{tid}");
    let stat = std::fs::read_to_string(format!("{root}/stat")).ok()?;
    let status = std::fs::read_to_string(format!("{root}/status")).ok()?;
    let (user_cpu_ticks, system_cpu_ticks, _) = parse_proc_stat(&stat)?;
    let (voluntary_context_switches, nonvoluntary_context_switches) =
        parse_context_switches(&status)?;
    Some(TaskPerformanceStatus {
        user_cpu_ticks,
        system_cpu_ticks,
        voluntary_context_switches,
        nonvoluntary_context_switches,
    })
}

#[cfg(not(target_os = "linux"))]
fn task_performance(_tid: u32) -> Option<TaskPerformanceStatus> {
    None
}

#[cfg(target_os = "linux")]
fn process_performance() -> Option<ProcessPerformanceStatus> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let (user_cpu_ticks, system_cpu_ticks, resident_pages) = parse_proc_stat(&stat)?;
    let resident_pages = u64::try_from(resident_pages).ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = u64::try_from(page_size).ok()?;
    let (voluntary_context_switches, nonvoluntary_context_switches) = process_context_switches()?;
    Some(ProcessPerformanceStatus {
        pid: std::process::id(),
        user_cpu_ticks,
        system_cpu_ticks,
        resident_memory_bytes: resident_pages.saturating_mul(page_size),
        voluntary_context_switches,
        nonvoluntary_context_switches,
    })
}

#[cfg(not(target_os = "linux"))]
fn process_performance() -> Option<ProcessPerformanceStatus> {
    None
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_second() -> u64 {
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(ticks).unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn clock_ticks_per_second() -> u64 {
    0
}

fn count_directory_entries(path: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let nested = entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .map(|_| count_directory_entries(&entry.path()))
                .unwrap_or(0);
            1 + nested
        })
        .sum()
}

fn bpffs_mounted() -> bool {
    std::fs::read_to_string("/proc/mounts")
        .ok()
        .is_some_and(|mounts| {
            mounts.lines().any(|line| {
                let mut fields = line.split_whitespace();
                matches!(
                    (fields.next(), fields.next(), fields.next()),
                    (Some(_), Some("/sys/fs/bpf"), Some("bpf"))
                )
            })
        })
}

/// Identity of a caller, returned by `resolve_identity()`.
#[derive(Debug, Clone)]
pub struct CallerIdentity {
    pub namespace: String,
    pub app_id: String,
    pub tid: u32,
}

/// Tracks when a port→TID binding was created.
struct PortBinding {
    tid: u32,
    created_at: Instant,
    epoch: u64,
}

/// Opaque proof captured when the gateway accepts a connection. It cannot be
/// transferred to a later connection that reuses the same source port.
#[derive(Clone)]
pub struct ConnectionSnapshot {
    peer: SocketAddr,
    owner: GatewayOwnerRef,
    identity: GatewayOwnerIdentity,
    kernel: bool,
}

#[cfg(any(feature = "ebpf", test))]
fn validate_kernel_owner(
    snapshot: &ConnectionSnapshot,
    live_owner: Option<GatewayOwnerIdentity>,
    current_identity: Option<TidIdentity>,
) -> Option<TidIdentity> {
    let owner = live_owner?;
    let identity = current_identity?;
    (owner == snapshot.identity
        && identity.registered_at_ns == owner.registered_at_ns
        && identity.flags == TidFlags::Enabled as u32)
        .then_some(identity)
}

#[cfg(feature = "ebpf")]
struct KernelConnections {
    peers: aya::maps::HashMap<aya::maps::MapData, GatewayPeerKey, GatewayOwnerRef>,
    owners: aya::maps::HashMap<aya::maps::MapData, GatewayOwnerRef, GatewayOwnerIdentity>,
    identity_map_index: usize,
}

/// Registered identities and authoritative kernel connection ownership.
/// Gateway proofs bind a socket owner to its TID registration generation.
/// Telemetry port bindings are retained for explicit fallback tests and metrics;
/// they never authorize callers in a map created by `from_ebpf`.
pub struct NamespaceMap {
    #[cfg(feature = "ebpf")]
    inner: Mutex<Vec<aya::maps::HashMap<aya::maps::MapData, u32, TidIdentity>>>,
    #[cfg(feature = "ebpf")]
    connections: Option<Mutex<KernelConnections>>,
    kernel_authority: bool,
    binding_epoch: AtomicU64,
    /// TID → TidIdentity. Always maintained. Primary identity store.
    tid_to_identity: RwLock<HashMap<u32, TidIdentity>>,
    /// Recently removed identities retained only for asynchronous event labeling.
    /// Security decisions never consult this table.
    recent_tid_identities: RwLock<HashMap<u32, (TidIdentity, Instant)>>,
    /// Source port → PortBinding. Populated by the consumer when eBPF events arrive.
    port_to_tid: RwLock<HashMap<u16, PortBinding>>,
    /// TTL for port→TID bindings. Bindings older than this are considered stale.
    port_ttl: Duration,
}

impl NamespaceMap {
    /// Create from a loaded eBPF object.
    ///
    /// The `MONITORED_TIDS` map is expected in the namespace enforcer eBPF
    /// object (`ns_ebpf`). Missing connection maps fail closed for gateway
    /// authorization; only `new_fallback` enables in-process test bindings.
    // TODO: When aya supports BPF_F_RDONLY_PROG, load the MONITORED_TIDS map
    // with read-only program access to prevent eBPF programs from mutating
    // the identity table. This is defense-in-depth — the eBPF programs
    // should never need to write to this map.
    #[cfg(feature = "ebpf")]
    pub fn from_ebpf(
        mut ns_ebpf: Option<&mut aya::Ebpf>,
        monitors: &mut [crate::loader::LoadedMonitor],
    ) -> Self {
        let mut inner = Vec::new();
        let peers = ns_ebpf
            .as_mut()
            .and_then(|bpf| bpf.take_map("GATEWAY_PEERS"))
            .and_then(|map| aya::maps::HashMap::try_from(map).ok());
        let owners = ns_ebpf
            .as_mut()
            .and_then(|bpf| bpf.take_map("GATEWAY_OWNERS"))
            .and_then(|map| aya::maps::HashMap::try_from(map).ok());
        let mut identity_map_index = None;

        for (owner, ebpf) in monitors
            .iter_mut()
            .filter(|monitor| {
                matches!(
                    monitor.name,
                    "process_tracker" | "tcp_monitor" | "fd_watcher" | "syscall_counter"
                )
            })
            .map(|monitor| (monitor.name, &mut monitor.ebpf))
            .chain(ns_ebpf.into_iter().map(|ebpf| ("namespace_enforcer", ebpf)))
        {
            let Some(map) = ebpf.take_map("MONITORED_TIDS") else {
                warn!(owner, "MONITORED_TIDS map not found");
                continue;
            };
            match aya::maps::HashMap::<aya::maps::MapData, u32, TidIdentity>::try_from(map) {
                Ok(hash_map) => {
                    if owner == "namespace_enforcer" {
                        identity_map_index = Some(inner.len());
                    }
                    info!(owner, "MONITORED_TIDS eBPF map opened");
                    inner.push(hash_map);
                }
                Err(e) => warn!(owner, error = %e, "MONITORED_TIDS map has the wrong type"),
            }
        }

        if inner.is_empty() {
            warn!("No MONITORED_TIDS eBPF maps opened; kernel gateway authorization unavailable");
        }

        NamespaceMap {
            inner: Mutex::new(inner),
            connections: peers.zip(owners).zip(identity_map_index).map(
                |((peers, owners), identity_map_index)| {
                    Mutex::new(KernelConnections {
                        peers,
                        owners,
                        identity_map_index,
                    })
                },
            ),
            kernel_authority: true,
            binding_epoch: AtomicU64::new(1),
            tid_to_identity: RwLock::new(HashMap::new()),
            recent_tid_identities: RwLock::new(HashMap::new()),
            port_to_tid: RwLock::new(HashMap::new()),
            port_ttl: Duration::from_secs(300),
        }
    }

    /// Create a fallback-only map (no eBPF).
    pub fn new_fallback() -> Self {
        NamespaceMap {
            #[cfg(feature = "ebpf")]
            inner: Mutex::new(Vec::new()),
            #[cfg(feature = "ebpf")]
            connections: None,
            kernel_authority: false,
            binding_epoch: AtomicU64::new(1),
            tid_to_identity: RwLock::new(HashMap::new()),
            recent_tid_identities: RwLock::new(HashMap::new()),
            port_to_tid: RwLock::new(HashMap::new()),
            port_ttl: Duration::from_secs(300),
        }
    }

    /// Register a TID with its namespace/app identity.
    ///
    /// Called from inside the `spawn_blocking` closure, after `gettid()`
    /// but before `instance.run()`.
    pub fn register_tid(&self, tid: u32, mut identity: TidIdentity) -> Result<(), String> {
        identity.flags = TidFlags::Enabled as u32;
        identity.registered_at_ns = Self::now_ns();

        #[cfg(feature = "ebpf")]
        {
            let mut maps = self.inner.lock().unwrap();
            for map in maps.iter_mut() {
                if let Err(e) = map.insert(tid, identity, 0) {
                    for inserted in maps.iter_mut() {
                        let _ = inserted.remove(&tid);
                    }
                    return Err(format!(
                        "failed to register TID {tid} in every eBPF map: {e}"
                    ));
                }
            }

            if !maps.is_empty() {
                info!(
                    tid,
                    map_count = maps.len(),
                    ns = identity.namespace_str(),
                    app = identity.app_id_str(),
                    "TID registered in eBPF maps"
                );
            }
        }

        self.recent_tid_identities.write().unwrap().remove(&tid);
        self.tid_to_identity.write().unwrap().insert(tid, identity);
        info!(
            tid,
            ns = identity.namespace_str(),
            app = identity.app_id_str(),
            "TID registered"
        );
        Ok(())
    }

    /// Deregister a TID from the map.
    ///
    /// Called when an instance is killed or exits.
    /// Also removes any port_to_tid entries for this TID.
    pub fn deregister_tid(&self, tid: u32) -> Result<(), String> {
        #[cfg(feature = "ebpf")]
        for map in self.inner.lock().unwrap().iter_mut() {
            let _ = map.remove(&tid);
        }

        if let Some(identity) = self.tid_to_identity.write().unwrap().remove(&tid) {
            self.recent_tid_identities
                .write()
                .unwrap()
                .insert(tid, (identity, Instant::now()));
        }

        // Remove any port→TID mappings for this TID
        let mut port_map = self.port_to_tid.write().unwrap();
        port_map.retain(|_, binding| binding.tid != tid);

        info!(tid, "TID deregistered");
        Ok(())
    }

    /// Bind a source port to a TID.
    ///
    /// Called when the eBPF consumer receives a TidConnection event,
    /// or when the Supervisor detects that a TID has established a
    /// TCP connection to the gateway from a specific source port.
    pub fn bind_port(&self, source_port: u16, tid: u32) {
        self.port_to_tid.write().unwrap().insert(
            source_port,
            PortBinding {
                tid,
                created_at: Instant::now(),
                epoch: self.binding_epoch.fetch_add(1, Ordering::Relaxed),
            },
        );
        tracing::debug!(source_port, tid, "Port bound to TID");
    }

    /// Unbind a source port (when the connection closes).
    pub fn unbind_port(&self, source_port: u16) {
        self.port_to_tid.write().unwrap().remove(&source_port);
        tracing::debug!(source_port, "Port unbound");
    }

    /// Resolve the identity of a caller by source port.
    ///
    /// This is the method the gateway calls for each incoming request.
    /// It performs a two-step lookup: source_port → TID → {namespace, app_id}.
    ///
    /// Returns `None` if the source port is not bound or the TID is not
    /// registered (unregistered connection — deny by default).
    pub fn resolve_identity(&self, source_port: u16) -> Option<CallerIdentity> {
        let snapshot = self.snapshot_connection(SocketAddr::from(([127, 0, 0, 1], source_port)))?;
        self.resolve_connection(&snapshot)
    }

    /// Capture the current kernel-owned connection, independently of telemetry.
    pub fn snapshot_connection(&self, peer: SocketAddr) -> Option<ConnectionSnapshot> {
        let loopback = match peer.ip() {
            std::net::IpAddr::V4(ip) => ip.is_loopback(),
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|ip| ip.is_loopback())
            }
        };
        if !loopback {
            return None;
        }
        if self.kernel_authority {
            #[cfg(feature = "ebpf")]
            {
                let maps = self.connections.as_ref()?.lock().ok()?;
                let owner = maps.peers.get(&GatewayPeerKey::from_peer(peer), 0).ok()?;
                if owner.socket_id == 0 || owner.connected_at_ns == 0 {
                    return None;
                }
                let identity = maps.owners.get(&owner, 0).ok()?;
                let snapshot = ConnectionSnapshot {
                    peer,
                    owner,
                    identity,
                    kernel: true,
                };
                drop(maps);
                self.resolve_connection(&snapshot)?;
                return Some(snapshot);
            }
            #[cfg(not(feature = "ebpf"))]
            return None;
        }
        let mut ports = self.port_to_tid.write().ok()?;
        let binding = ports.get(&peer.port())?;
        if binding.created_at.elapsed() > self.port_ttl {
            ports.remove(&peer.port());
            return None;
        }
        let identity = self.lookup_tid(binding.tid)?;
        Some(ConnectionSnapshot {
            peer,
            owner: GatewayOwnerRef {
                socket_id: binding.epoch,
                connected_at_ns: 0,
            },
            identity: GatewayOwnerIdentity {
                registered_at_ns: identity.registered_at_ns,
                tid: binding.tid,
                _padding: 0,
            },
            kernel: false,
        })
    }

    /// Revalidate the accepted socket and TID generation for every request.
    pub fn resolve_connection(&self, snapshot: &ConnectionSnapshot) -> Option<CallerIdentity> {
        if snapshot.identity.tid == 0 || snapshot.identity.registered_at_ns == 0 {
            return None;
        }
        if snapshot.kernel != self.kernel_authority {
            return None;
        }
        if self.kernel_authority {
            #[cfg(feature = "ebpf")]
            {
                let maps = self.connections.as_ref()?.lock().ok()?;
                let owner = maps.owners.get(&snapshot.owner, 0).ok()?;
                if owner != snapshot.identity {
                    return None;
                }
                let identities = self.inner.lock().ok()?;
                let identity = identities
                    .get(maps.identity_map_index)?
                    .get(&owner.tid, 0)
                    .ok()?;
                validate_kernel_owner(snapshot, Some(owner), Some(identity))?;
            }
            #[cfg(not(feature = "ebpf"))]
            return None;
        } else {
            let ports = self.port_to_tid.read().ok()?;
            let binding = ports.get(&snapshot.peer.port())?;
            if binding.epoch != snapshot.owner.socket_id
                || binding.tid != snapshot.identity.tid
                || binding.created_at.elapsed() > self.port_ttl
            {
                return None;
            }
        }
        let identity = self.lookup_tid(snapshot.identity.tid)?;
        if identity.registered_at_ns != snapshot.identity.registered_at_ns
            || identity.flags != TidFlags::Enabled as u32
        {
            return None;
        }
        Some(CallerIdentity {
            namespace: identity.namespace_str().to_owned(),
            app_id: identity.app_id_str().to_owned(),
            tid: snapshot.identity.tid,
        })
    }

    /// Look up a TID's identity directly (for admin/debug API).
    pub fn lookup_tid(&self, tid: u32) -> Option<TidIdentity> {
        self.tid_to_identity.read().unwrap().get(&tid).copied()
    }

    /// Return a consistent, read-only lifecycle snapshot for operators/tests.
    pub fn status(&self) -> NamespaceMapStatus {
        let mut active_tids: Vec<_> = self
            .tid_to_identity
            .read()
            .unwrap()
            .iter()
            .map(|(tid, identity)| MonitoredTidStatus {
                tid: *tid,
                namespace: identity.namespace_str().to_string(),
                app_id: identity.app_id_str().to_string(),
                registered_at_ns: identity.registered_at_ns,
                performance: task_performance(*tid),
            })
            .collect();
        active_tids.sort_by_key(|identity| identity.tid);

        let recent_tombstones = {
            let mut recent = self.recent_tid_identities.write().unwrap();
            recent.retain(|_, (_, removed_at)| removed_at.elapsed() <= Duration::from_secs(30));
            recent.len()
        };

        #[cfg(feature = "ebpf")]
        let kernel_map_entry_counts = self
            .inner
            .lock()
            .unwrap()
            .iter()
            .map(|map| map.keys().filter(Result::is_ok).count())
            .collect();
        #[cfg(not(feature = "ebpf"))]
        let kernel_map_entry_counts = Vec::new();

        NamespaceMapStatus {
            active_tids,
            clock_ticks_per_second: clock_ticks_per_second(),
            process_performance: process_performance(),
            recent_tombstones,
            port_bindings: self.port_to_tid.read().unwrap().len(),
            kernel_map_entry_counts,
            bpffs_mounted: bpffs_mounted(),
            pinned_entries: count_directory_entries(std::path::Path::new("/sys/fs/bpf")),
        }
    }

    /// Label an asynchronously consumed event, including a short-lived identity
    /// tombstone for events emitted immediately before thread teardown.
    pub fn lookup_event_identity(&self, tid: u32) -> Option<TidIdentity> {
        if let Some(identity) = self.lookup_tid(tid) {
            return Some(identity);
        }
        let mut recent = self.recent_tid_identities.write().unwrap();
        recent.retain(|_, (_, removed_at)| removed_at.elapsed() <= Duration::from_secs(30));
        recent.get(&tid).map(|(identity, _)| *identity)
    }

    /// Remove port bindings that have exceeded their TTL.
    /// Returns the number of bindings removed.
    pub fn cleanup_expired_bindings(&self) -> usize {
        let mut port_map = self.port_to_tid.write().unwrap();
        let before = port_map.len();
        port_map.retain(|port, binding| {
            let alive = binding.created_at.elapsed() <= self.port_ttl;
            if !alive {
                tracing::debug!(port, tid = binding.tid, "Expired port binding cleaned up");
            }
            alive
        });
        let removed = before - port_map.len();
        if removed > 0 {
            tracing::info!(removed, "Cleaned up expired port→TID bindings");
        }
        removed
    }

    /// Set the TTL for port→TID bindings.
    pub fn set_port_ttl(&mut self, ttl: Duration) {
        self.port_ttl = ttl;
    }

    /// Cleanup stale TIDs whose threads no longer exist.
    ///
    /// Called periodically by the Supervisor's health loop.
    pub fn cleanup_stale_tids(&self) -> usize {
        self.recent_tid_identities
            .write()
            .unwrap()
            .retain(|_, (_, removed_at)| removed_at.elapsed() <= Duration::from_secs(30));
        let tids: Vec<u32> = self
            .tid_to_identity
            .read()
            .unwrap()
            .keys()
            .copied()
            .collect();
        let mut removed = 0;

        for tid in tids {
            if !Self::is_tid_alive(tid) {
                warn!(tid, "Cleaning up stale TID");
                let _ = self.deregister_tid(tid);
                removed += 1;
            }
        }

        removed
    }

    #[cfg(target_os = "linux")]
    fn is_tid_alive(tid: u32) -> bool {
        unsafe {
            let ret = libc::kill(tid as i32, 0);
            ret == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn is_tid_alive(_tid: u32) -> bool {
        true
    }

    #[cfg(target_os = "linux")]
    fn now_ns() -> u64 {
        let mut ts = std::mem::MaybeUninit::<libc::timespec>::uninit();
        unsafe {
            libc::clock_gettime(libc::CLOCK_MONOTONIC, ts.as_mut_ptr());
            let ts = ts.assume_init();
            (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn now_ns() -> u64 {
        use std::time::Instant;
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let start = START.get_or_init(Instant::now);
        start.elapsed().as_nanos() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_does_not_inherit_rebound_port_or_tid_generation() {
        let map = NamespaceMap::new_fallback();
        map.register_tid(42, TidIdentity::new("a", "caller:v1"))
            .unwrap();
        let peer = "127.0.0.1:54321".parse().unwrap();
        map.bind_port(54321, 42);
        let old = map.snapshot_connection(peer).unwrap();
        assert!(map.resolve_connection(&old).is_some());
        map.bind_port(54321, 42);
        assert!(map.resolve_connection(&old).is_none());
        let next = map.snapshot_connection(peer).unwrap();
        map.deregister_tid(42).unwrap();
        map.register_tid(42, TidIdentity::new("b", "replacement:v1"))
            .unwrap();
        map.bind_port(54321, 42);
        assert!(map.resolve_connection(&next).is_none());
        assert_eq!(map.resolve_identity(54321).unwrap().namespace, "b");
    }

    #[test]
    fn kernel_mode_never_authorizes_telemetry_only_binding() {
        let mut map = NamespaceMap::new_fallback();
        map.register_tid(42, TidIdentity::new("a", "caller:v1"))
            .unwrap();
        map.bind_port(54321, 42);
        let fallback = map
            .snapshot_connection("127.0.0.1:54321".parse().unwrap())
            .unwrap();
        map.kernel_authority = true;
        assert!(map
            .snapshot_connection("127.0.0.1:54321".parse().unwrap())
            .is_none());
        assert!(map.resolve_connection(&fallback).is_none());
    }

    #[test]
    fn kernel_live_owner_and_registration_are_both_required() {
        let mut current = TidIdentity::new("a", "caller:v1");
        current.flags = TidFlags::Enabled as u32;
        current.registered_at_ns = 123;
        let owner = GatewayOwnerIdentity {
            registered_at_ns: 123,
            tid: 42,
            _padding: 0,
        };
        let snapshot = ConnectionSnapshot {
            peer: "127.0.0.1:54321".parse().unwrap(),
            owner: GatewayOwnerRef {
                socket_id: 99,
                connected_at_ns: 456,
            },
            identity: owner,
            kernel: true,
        };
        assert!(validate_kernel_owner(&snapshot, Some(owner), Some(current)).is_some());
        // Close removes this unique owner; a new peer-index value cannot revive it.
        assert!(validate_kernel_owner(&snapshot, None, Some(current)).is_none());
        let replacement = GatewayOwnerIdentity {
            registered_at_ns: 124,
            ..owner
        };
        assert!(validate_kernel_owner(&snapshot, Some(replacement), Some(current)).is_none());
        current.registered_at_ns = 124;
        assert!(validate_kernel_owner(&snapshot, Some(owner), Some(current)).is_none());
        assert!(validate_kernel_owner(&snapshot, Some(owner), None).is_none());
    }

    #[test]
    fn test_register_and_resolve() {
        let map = NamespaceMap::new_fallback();
        let identity = TidIdentity::new("production", "payments:v1");
        map.register_tid(12345, identity).unwrap();

        // Without port binding, resolve should return None
        assert!(map.resolve_identity(54321).is_none());

        // Bind the port and resolve
        map.bind_port(54321, 12345);
        let resolved = map.resolve_identity(54321).unwrap();
        assert_eq!(resolved.namespace, "production");
        assert_eq!(resolved.app_id, "payments:v1");
        assert_eq!(resolved.tid, 12345);
    }

    #[test]
    fn test_resolve_unknown_port() {
        let map = NamespaceMap::new_fallback();
        assert!(map.resolve_identity(9999).is_none());
    }

    #[test]
    fn test_deregister_removes_identity() {
        let map = NamespaceMap::new_fallback();
        let identity = TidIdentity::new("staging", "api:v1");
        map.register_tid(12346, identity).unwrap();
        map.bind_port(54322, 12346);

        assert!(map.resolve_identity(54322).is_some());

        map.deregister_tid(12346).unwrap();
        assert!(map.resolve_identity(54322).is_none());
        assert!(map.lookup_tid(12346).is_none());
    }

    #[test]
    fn test_deregister_cleans_ports() {
        let map = NamespaceMap::new_fallback();
        let identity = TidIdentity::new("default", "app:v1");
        map.register_tid(12347, identity).unwrap();
        map.bind_port(54323, 12347);
        map.bind_port(54324, 12347);

        map.deregister_tid(12347).unwrap();

        assert!(map.resolve_identity(54323).is_none());
        assert!(map.resolve_identity(54324).is_none());
    }

    #[test]
    fn test_unbind_port() {
        let map = NamespaceMap::new_fallback();
        let identity = TidIdentity::new("default", "app:v1");
        map.register_tid(12348, identity).unwrap();
        map.bind_port(54325, 12348);

        assert!(map.resolve_identity(54325).is_some());

        map.unbind_port(54325);
        assert!(map.resolve_identity(54325).is_none());
    }

    #[test]
    fn test_lookup_tid() {
        let map = NamespaceMap::new_fallback();
        let identity = TidIdentity::new("prod", "svc:v2");
        map.register_tid(12349, identity).unwrap();

        let found = map.lookup_tid(12349).unwrap();
        assert_eq!(found.namespace_str(), "prod");
        assert_eq!(found.app_id_str(), "svc:v2");
    }

    #[test]
    fn test_status_tracks_registration_and_cleanup() {
        let map = NamespaceMap::new_fallback();
        map.register_tid(12351, TidIdentity::new("prod", "lifecycle:v1"))
            .unwrap();
        map.bind_port(54327, 12351);

        let active = map.status();
        assert_eq!(active.active_tids.len(), 1);
        assert_eq!(active.active_tids[0].tid, 12351);
        assert_eq!(active.active_tids[0].namespace, "prod");
        assert_eq!(active.active_tids[0].app_id, "lifecycle:v1");
        assert_eq!(active.port_bindings, 1);

        map.deregister_tid(12351).unwrap();
        let removed = map.status();
        assert!(removed.active_tids.is_empty());
        assert_eq!(removed.recent_tombstones, 1);
        assert_eq!(removed.port_bindings, 0);
    }

    #[test]
    fn test_parse_proc_stat_with_parenthesized_command() {
        let stat = "42 (worker (one)) R 1 2 3 4 5 6 7 8 9 10 120 30 14 15 16 17 18 19 20 21 64";
        assert_eq!(parse_proc_stat(stat), Some((120, 30, 64)));
    }

    #[test]
    fn test_parse_context_switches() {
        let status =
            "Name:\tworker\nvoluntary_ctxt_switches:\t123\nnonvoluntary_ctxt_switches:\t45\n";
        assert_eq!(parse_context_switches(status), Some((123, 45)));
    }

    #[test]
    fn test_cleanup_stale_tids() {
        let map = NamespaceMap::new_fallback();
        // Register a TID that doesn't exist (very high number)
        let identity = TidIdentity::new("test", "app:v1");
        map.register_tid(999_999, identity).unwrap();

        let _removed = map.cleanup_stale_tids();
        // On Linux, this TID likely doesn't exist so it should be removed.
        // On non-Linux, is_tid_alive returns true, so nothing is removed.
        #[cfg(target_os = "linux")]
        assert!(_removed >= 1 || map.lookup_tid(999_999).is_none());
    }

    #[test]
    fn test_port_binding_expires() {
        let mut map = NamespaceMap::new_fallback();
        map.set_port_ttl(Duration::from_millis(10));

        let identity = TidIdentity::new("default", "app:v1");
        map.register_tid(12350, identity).unwrap();
        map.bind_port(54326, 12350);

        // Should resolve immediately
        assert!(map.resolve_identity(54326).is_some());

        // Wait for TTL to expire
        std::thread::sleep(Duration::from_millis(20));

        // Should now return None and remove the binding
        assert!(map.resolve_identity(54326).is_none());

        // Cleanup should find nothing (already removed by resolve_identity)
        let removed = map.cleanup_expired_bindings();
        assert_eq!(removed, 0);
    }
}

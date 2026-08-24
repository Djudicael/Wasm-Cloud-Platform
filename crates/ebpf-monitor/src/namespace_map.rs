//! Userspace API for the MONITORED_TIDS eBPF map.
//!
//! Provides `register_tid` and `deregister_tid` operations that the
//! Supervisor calls when spawning/killing instances.
//! Also provides the `resolve_identity` method that the gateway calls
//! to answer "who is calling from source port X?"

use crate::common::{TidFlags, TidIdentity};
use std::collections::HashMap;
#[cfg(feature = "ebpf")]
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tracing::{info, warn};

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
}

/// Handle to the MONITORED_TIDS eBPF map + in-process identity tables.
///
/// This is the single source of truth for identity resolution. The gateway
/// calls `resolve_identity(source_port)` and gets back a `CallerIdentity`.
///
/// Two maps are maintained:
/// - `tid_to_identity`: TID → {namespace, app_id} (populated by register_tid)
/// - `port_to_tid`: source_port → TID (populated when connections are tracked)
///
/// When the `ebpf` feature is not enabled, all operations use in-process
/// fallback maps. The gateway reads from the same maps for identity resolution.
pub struct NamespaceMap {
    #[cfg(feature = "ebpf")]
    inner: Mutex<Option<aya::maps::HashMap<aya::maps::MapData, u32, TidIdentity>>>,
    /// TID → TidIdentity. Always maintained. Primary identity store.
    tid_to_identity: RwLock<HashMap<u32, TidIdentity>>,
    /// Source port → PortBinding. Populated by the consumer when eBPF events arrive.
    port_to_tid: RwLock<HashMap<u16, PortBinding>>,
    /// TTL for port→TID bindings. Bindings older than this are considered stale.
    port_ttl: Duration,
}

impl NamespaceMap {
    /// Create from a loaded eBPF object.
    ///
    /// The `MONITORED_TIDS` map is expected in the namespace enforcer eBPF
    /// object (`ns_ebpf`). If not available, falls back to in-process maps.
    // TODO: When aya supports BPF_F_RDONLY_PROG, load the MONITORED_TIDS map
    // with read-only program access to prevent eBPF programs from mutating
    // the identity table. This is defense-in-depth — the eBPF programs
    // should never need to write to this map.
    #[cfg(feature = "ebpf")]
    pub fn from_ebpf(ns_ebpf: Option<&mut aya::Ebpf>) -> Self {
        let inner = if let Some(ns) = ns_ebpf {
            match ns.take_map("MONITORED_TIDS") {
                Some(map) => {
                    match aya::maps::HashMap::<aya::maps::MapData, u32, TidIdentity>::try_from(map)
                    {
                        Ok(hash_map) => {
                            info!("MONITORED_TIDS eBPF map opened from namespace enforcer");
                            Some(hash_map)
                        }
                        Err(e) => {
                            warn!(error = %e, "MONITORED_TIDS map wrong type — using fallback");
                            None
                        }
                    }
                }
                None => {
                    warn!("MONITORED_TIDS map not found in namespace enforcer — using fallback");
                    None
                }
            }
        } else {
            None
        };

        NamespaceMap {
            inner: Mutex::new(inner),
            tid_to_identity: RwLock::new(HashMap::new()),
            port_to_tid: RwLock::new(HashMap::new()),
            port_ttl: Duration::from_secs(300),
        }
    }

    /// Create a fallback-only map (no eBPF).
    pub fn new_fallback() -> Self {
        NamespaceMap {
            #[cfg(feature = "ebpf")]
            inner: Mutex::new(None),
            tid_to_identity: RwLock::new(HashMap::new()),
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
        if let Some(ref mut map) = *self.inner.lock().unwrap() {
            match map.insert(tid, identity, 0) {
                Ok(()) => {
                    info!(
                        tid,
                        ns = identity.namespace_str(),
                        app = identity.app_id_str(),
                        "TID registered in eBPF map"
                    );
                    self.tid_to_identity.write().unwrap().insert(tid, identity);
                    return Ok(());
                }
                Err(e) => {
                    warn!(tid, error = %e, "eBPF map insert failed — using fallback");
                }
            }
        }

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
        if let Some(ref mut map) = *self.inner.lock().unwrap() {
            let _ = map.remove(&tid);
        }

        self.tid_to_identity.write().unwrap().remove(&tid);

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
        let mut port_map = self.port_to_tid.write().unwrap();
        let binding = port_map.get(&source_port)?;

        // Check TTL
        if binding.created_at.elapsed() > self.port_ttl {
            tracing::warn!(
                source_port,
                tid = binding.tid,
                "Port binding expired — removing"
            );
            port_map.remove(&source_port);
            return None;
        }

        let tid = binding.tid;
        drop(port_map);

        let tid_map = self.tid_to_identity.read().unwrap();
        let identity = tid_map.get(&tid)?;

        Some(CallerIdentity {
            namespace: identity.namespace_str().to_string(),
            app_id: identity.app_id_str().to_string(),
            tid,
        })
    }

    /// Look up a TID's identity directly (for admin/debug API).
    pub fn lookup_tid(&self, tid: u32) -> Option<TidIdentity> {
        self.tid_to_identity.read().unwrap().get(&tid).copied()
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

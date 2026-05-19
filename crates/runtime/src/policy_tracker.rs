//! Policy enforcement tracker for WASI host functions.
//!
//! This module provides the `PolicyEnforcer` which checks network and filesystem
//! operations against a per-instance policy, and `PolicyCounters` which tracks
//! usage and violations atomically.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use common::policy::InstancePolicy;

/// Atomic counters for a single instance's policy enforcement.
/// Shared between the WASI host functions (which increment) and
/// the metrics exporter (which reads).
#[derive(Debug)]
pub struct PolicyCounters {
    // Network counters
    pub outbound_connections_active: AtomicU32,
    pub outbound_connections_total: AtomicU64,
    pub egress_bytes: AtomicU64,
    pub dns_lookups_total: AtomicU64,
    pub inbound_connections_active: AtomicU32,

    // Filesystem counters
    pub open_fds: AtomicU32,
    pub fd_open_total: AtomicU64,
    pub fs_write_bytes: AtomicU64,
    pub fs_read_bytes: AtomicU64,
    pub file_creates_total: AtomicU64,
    pub file_deletes_total: AtomicU64,

    // Violation counters
    pub connection_denied_total: AtomicU64,
    pub egress_denied_total: AtomicU64,
    pub fd_denied_total: AtomicU64,
    pub fs_write_denied_total: AtomicU64,
    pub bind_denied_total: AtomicU64,
    pub dns_denied_total: AtomicU64,
}

impl PolicyCounters {
    pub fn new() -> Self {
        PolicyCounters {
            outbound_connections_active: AtomicU32::new(0),
            outbound_connections_total: AtomicU64::new(0),
            egress_bytes: AtomicU64::new(0),
            dns_lookups_total: AtomicU64::new(0),
            inbound_connections_active: AtomicU32::new(0),
            open_fds: AtomicU32::new(0),
            fd_open_total: AtomicU64::new(0),
            fs_write_bytes: AtomicU64::new(0),
            fs_read_bytes: AtomicU64::new(0),
            file_creates_total: AtomicU64::new(0),
            file_deletes_total: AtomicU64::new(0),
            connection_denied_total: AtomicU64::new(0),
            egress_denied_total: AtomicU64::new(0),
            fd_denied_total: AtomicU64::new(0),
            fs_write_denied_total: AtomicU64::new(0),
            bind_denied_total: AtomicU64::new(0),
            dns_denied_total: AtomicU64::new(0),
        }
    }
}

/// The policy enforcement engine. Lives in StoreState.
/// Called by custom WASI host functions before delegating to the real implementation.
pub struct PolicyEnforcer {
    pub policy: InstancePolicy,
    pub counters: Arc<PolicyCounters>,
    /// Pre-parsed allowed CIDRs (parsed once at construction).
    allowed_cidrs_parsed: Vec<ipnet::IpNet>,
    /// Pre-parsed denied CIDRs (parsed once at construction).
    denied_cidrs_parsed: Vec<ipnet::IpNet>,
}

impl std::fmt::Debug for PolicyEnforcer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyEnforcer")
            .field("policy", &self.policy)
            .field("allowed_cidrs", &self.allowed_cidrs_parsed)
            .field("denied_cidrs", &self.denied_cidrs_parsed)
            .finish_non_exhaustive()
    }
}

impl PolicyEnforcer {
    pub fn new(policy: InstancePolicy) -> Self {
        let allowed_cidrs_parsed = Self::parse_cidrs(&policy.network.allowed_cidrs);
        let denied_cidrs_parsed = Self::parse_cidrs(&policy.network.denied_cidrs);
        PolicyEnforcer {
            policy,
            counters: Arc::new(PolicyCounters::new()),
            allowed_cidrs_parsed,
            denied_cidrs_parsed,
        }
    }

    /// Parse CIDR strings into IpNet values, logging warnings for invalid entries.
    fn parse_cidrs(cidrs: &[String]) -> Vec<ipnet::IpNet> {
        cidrs
            .iter()
            .filter_map(|s| {
                s.parse::<ipnet::IpNet>().ok().or_else(|| {
                    tracing::warn!("Invalid CIDR string: {}, skipping", s);
                    None
                })
            })
            .collect()
    }

    /// Check if an outbound TCP connection is allowed and atomically reserve a slot.
    /// Uses compare_exchange to avoid TOCTOU races between the check and increment.
    pub fn check_outbound_tcp_connect(
        &self,
        dest_ip: IpAddr,
        _dest_port: u16,
    ) -> Result<(), PolicyDenied> {
        if !self.policy.network.allow_outbound_tcp {
            self.counters
                .connection_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::NetworkDisabled { protocol: "tcp" });
        }

        // Check denied CIDRs first (takes precedence)
        if Self::ip_in_cidrs(dest_ip, &self.denied_cidrs_parsed) {
            self.counters
                .connection_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::DestinationDenied {
                ip: dest_ip.to_string(),
                reason: "destination in denied_cidrs".to_string(),
            });
        }

        // Check allowed CIDRs (if non-empty, only these are allowed)
        if !self.allowed_cidrs_parsed.is_empty()
            && !Self::ip_in_cidrs(dest_ip, &self.allowed_cidrs_parsed)
        {
            self.counters
                .connection_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::DestinationDenied {
                ip: dest_ip.to_string(),
                reason: "destination not in allowed_cidrs".to_string(),
            });
        }

        // Atomically check connection count and reserve a slot
        let limit = self.policy.network.max_outbound_connections;
        loop {
            let current = self
                .counters
                .outbound_connections_active
                .load(Ordering::Acquire);
            if current >= limit {
                self.counters
                    .connection_denied_total
                    .fetch_add(1, Ordering::Relaxed);
                return Err(PolicyDenied::ConnectionLimitExceeded { current, limit });
            }
            if self
                .counters
                .outbound_connections_active
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        Ok(())
    }

    /// Record that an outbound connection was established.
    /// Note: `outbound_connections_active` is already incremented by `check_outbound_tcp_connect`.
    pub fn record_outbound_connect(&self) {
        self.counters
            .outbound_connections_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record that an outbound connection was closed.
    /// Guards against underflow by correcting back to 0 if needed.
    pub fn record_outbound_disconnect(&self) {
        let prev = self
            .counters
            .outbound_connections_active
            .fetch_sub(1, Ordering::AcqRel);
        if prev == 0 {
            // Underflow — correct back to 0
            self.counters
                .outbound_connections_active
                .store(0, Ordering::Release);
        }
    }

    /// Check if egress data is allowed (before sending).
    #[deprecated(
        since = "0.2.0",
        note = "Use check_and_record_egress instead to avoid TOCTOU races"
    )]
    pub fn check_egress(&self, additional_bytes: u64) -> Result<(), PolicyDenied> {
        if self.policy.network.max_egress_bytes == 0 {
            return Ok(()); // unlimited
        }

        let current = self.counters.egress_bytes.load(Ordering::Relaxed);
        if current + additional_bytes > self.policy.network.max_egress_bytes {
            self.counters
                .egress_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::EgressLimitExceeded {
                current,
                requested: additional_bytes,
                limit: self.policy.network.max_egress_bytes,
            });
        }

        Ok(())
    }

    /// Record egress bytes after a successful send.
    #[deprecated(
        since = "0.2.0",
        note = "Use check_and_record_egress instead to avoid TOCTOU races"
    )]
    pub fn record_egress(&self, bytes: u64) {
        self.counters
            .egress_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Atomically check if egress data is allowed and record the bytes.
    /// Uses compare_exchange to avoid TOCTOU races between the check and increment.
    pub fn check_and_record_egress(&self, bytes: u64) -> Result<(), PolicyDenied> {
        let limit = self.policy.network.max_egress_bytes;
        if limit == 0 {
            // unlimited — just record
            self.counters
                .egress_bytes
                .fetch_add(bytes, Ordering::Relaxed);
            return Ok(());
        }

        loop {
            let current = self.counters.egress_bytes.load(Ordering::Acquire);
            let new_val = current + bytes;
            if new_val > limit {
                self.counters
                    .egress_denied_total
                    .fetch_add(1, Ordering::Relaxed);
                return Err(PolicyDenied::EgressLimitExceeded {
                    current,
                    requested: bytes,
                    limit,
                });
            }
            if self
                .counters
                .egress_bytes
                .compare_exchange(current, new_val, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        Ok(())
    }

    /// Check if a DNS lookup is allowed.
    pub fn check_dns_lookup(&self) -> Result<(), PolicyDenied> {
        if !self.policy.network.allow_dns {
            self.counters
                .dns_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::DnsDisabled);
        }
        self.counters
            .dns_lookups_total
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Check if binding to a specific port is allowed.
    pub fn check_bind(&self, port: u16) -> Result<(), PolicyDenied> {
        if self.policy.network.allowed_bind_ports.contains(&port) {
            return Ok(());
        }
        self.counters
            .bind_denied_total
            .fetch_add(1, Ordering::Relaxed);
        Err(PolicyDenied::BindDenied {
            port,
            allowed: self.policy.network.allowed_bind_ports.clone(),
        })
    }

    // ── Filesystem Policy Checks ───────────────────────────────────

    /// Check if opening a file descriptor is allowed and atomically reserve a slot.
    /// Uses compare_exchange to avoid TOCTOU races between the check and increment.
    pub fn check_fd_open(&self) -> Result<(), PolicyDenied> {
        let limit = self.policy.filesystem.max_open_fds;
        loop {
            let current = self.counters.open_fds.load(Ordering::Acquire);
            if current >= limit {
                self.counters
                    .fd_denied_total
                    .fetch_add(1, Ordering::Relaxed);
                return Err(PolicyDenied::FdLimitExceeded { current, limit });
            }
            if self
                .counters
                .open_fds
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
        Ok(())
    }

    /// Record that an FD was opened.
    /// Note: `open_fds` is already incremented by `check_fd_open`.
    pub fn record_fd_open(&self) {
        self.counters.fd_open_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that an FD was closed.
    /// Guards against underflow by correcting back to 0 if needed.
    pub fn record_fd_close(&self) {
        let prev = self.counters.open_fds.fetch_sub(1, Ordering::AcqRel);
        if prev == 0 {
            // Underflow — correct back to 0
            self.counters.open_fds.store(0, Ordering::Release);
        }
    }

    /// Check if a filesystem write is allowed.
    #[deprecated(
        since = "0.2.0",
        note = "Use check_and_record_fs_write instead to avoid TOCTOU races"
    )]
    pub fn check_fs_write(&self, additional_bytes: u64) -> Result<(), PolicyDenied> {
        if self.policy.filesystem.max_fs_write_bytes == 0 {
            return Ok(()); // unlimited
        }

        let current = self.counters.fs_write_bytes.load(Ordering::Relaxed);
        if current + additional_bytes > self.policy.filesystem.max_fs_write_bytes {
            self.counters
                .fs_write_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::FsWriteLimitExceeded {
                current,
                requested: additional_bytes,
                limit: self.policy.filesystem.max_fs_write_bytes,
            });
        }
        Ok(())
    }

    /// Record filesystem write bytes.
    #[deprecated(
        since = "0.2.0",
        note = "Use check_and_record_fs_write instead to avoid TOCTOU races"
    )]
    pub fn record_fs_write(&self, bytes: u64) {
        self.counters
            .fs_write_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Atomically check if a filesystem write is allowed and record the bytes.
    /// Uses compare_exchange to avoid TOCTOU races between the check and increment.
    pub fn check_and_record_fs_write(&self, bytes: u64) -> Result<(), PolicyDenied> {
        let limit = self.policy.filesystem.max_fs_write_bytes;
        if limit == 0 {
            // unlimited — just record
            self.counters
                .fs_write_bytes
                .fetch_add(bytes, Ordering::Relaxed);
            return Ok(());
        }

        loop {
            let current = self.counters.fs_write_bytes.load(Ordering::Acquire);
            let new_val = current + bytes;
            if new_val > limit {
                self.counters
                    .fs_write_denied_total
                    .fetch_add(1, Ordering::Relaxed);
                return Err(PolicyDenied::FsWriteLimitExceeded {
                    current,
                    requested: bytes,
                    limit,
                });
            }
            if self
                .counters
                .fs_write_bytes
                .compare_exchange(current, new_val, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        Ok(())
    }

    /// Check if creating a file is allowed.
    pub fn check_file_create(&self) -> Result<(), PolicyDenied> {
        if !self.policy.filesystem.allow_file_create {
            return Err(PolicyDenied::FileCreateDenied);
        }
        self.counters
            .file_creates_total
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Check if deleting a file is allowed.
    pub fn check_file_delete(&self) -> Result<(), PolicyDenied> {
        if !self.policy.filesystem.allow_file_delete {
            return Err(PolicyDenied::FileDeleteDenied);
        }
        self.counters
            .file_deletes_total
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    // ── Helpers ────────────────────────────────────────────────────

    /// Check if an IP address falls within any of the given pre-parsed CIDRs.
    fn ip_in_cidrs(ip: IpAddr, cidrs: &[ipnet::IpNet]) -> bool {
        cidrs.iter().any(|cidr| cidr.contains(&ip))
    }
}

/// Reason a policy check denied an operation.
/// Returned as an error from WASI host functions.
#[derive(Debug, Clone)]
pub enum PolicyDenied {
    NetworkDisabled {
        protocol: &'static str,
    },
    DestinationDenied {
        ip: String,
        reason: String,
    },
    ConnectionLimitExceeded {
        current: u32,
        limit: u32,
    },
    EgressLimitExceeded {
        current: u64,
        requested: u64,
        limit: u64,
    },
    DnsDisabled,
    BindDenied {
        port: u16,
        allowed: Vec<u16>,
    },
    FdLimitExceeded {
        current: u32,
        limit: u32,
    },
    FsWriteLimitExceeded {
        current: u64,
        requested: u64,
        limit: u64,
    },
    FileCreateDenied,
    FileDeleteDenied,
}

impl std::fmt::Display for PolicyDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyDenied::NetworkDisabled { protocol } => {
                write!(
                    f,
                    "outbound {} connections are disabled by policy",
                    protocol
                )
            }
            PolicyDenied::DestinationDenied { ip, reason } => {
                write!(f, "connection to {} denied: {}", ip, reason)
            }
            PolicyDenied::ConnectionLimitExceeded { current, limit } => {
                write!(
                    f,
                    "outbound connection limit exceeded ({}/{})",
                    current, limit
                )
            }
            PolicyDenied::EgressLimitExceeded {
                current,
                requested,
                limit,
            } => {
                write!(
                    f,
                    "egress limit exceeded ({}+{} > {})",
                    current, requested, limit
                )
            }
            PolicyDenied::DnsDisabled => {
                write!(f, "DNS lookups are disabled by policy")
            }
            PolicyDenied::BindDenied { port, allowed } => {
                write!(
                    f,
                    "binding to port {} denied (allowed: {:?})",
                    port, allowed
                )
            }
            PolicyDenied::FdLimitExceeded { current, limit } => {
                write!(f, "FD limit exceeded ({}/{})", current, limit)
            }
            PolicyDenied::FsWriteLimitExceeded {
                current,
                requested,
                limit,
            } => {
                write!(
                    f,
                    "filesystem write limit exceeded ({}+{} > {})",
                    current, requested, limit
                )
            }
            PolicyDenied::FileCreateDenied => {
                write!(f, "file creation is disabled by policy")
            }
            PolicyDenied::FileDeleteDenied => {
                write!(f, "file deletion is disabled by policy")
            }
        }
    }
}

impl std::error::Error for PolicyDenied {}

#[cfg(test)]
mod tests {
    use super::*;
    use common::policy::{FilesystemPolicy, InstancePolicy, NetworkPolicy};

    fn make_policy() -> InstancePolicy {
        InstancePolicy {
            network: NetworkPolicy::default(),
            filesystem: FilesystemPolicy::default(),
        }
    }

    #[test]
    fn test_policy_counters_new() {
        let counters = PolicyCounters::new();
        assert_eq!(
            counters.outbound_connections_active.load(Ordering::Relaxed),
            0
        );
        assert_eq!(
            counters.outbound_connections_total.load(Ordering::Relaxed),
            0
        );
        assert_eq!(counters.egress_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(counters.dns_lookups_total.load(Ordering::Relaxed), 0);
        assert_eq!(
            counters.inbound_connections_active.load(Ordering::Relaxed),
            0
        );
        assert_eq!(counters.open_fds.load(Ordering::Relaxed), 0);
        assert_eq!(counters.fd_open_total.load(Ordering::Relaxed), 0);
        assert_eq!(counters.fs_write_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(counters.fs_read_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(counters.file_creates_total.load(Ordering::Relaxed), 0);
        assert_eq!(counters.file_deletes_total.load(Ordering::Relaxed), 0);
        assert_eq!(counters.connection_denied_total.load(Ordering::Relaxed), 0);
        assert_eq!(counters.egress_denied_total.load(Ordering::Relaxed), 0);
        assert_eq!(counters.fd_denied_total.load(Ordering::Relaxed), 0);
        assert_eq!(counters.fs_write_denied_total.load(Ordering::Relaxed), 0);
        assert_eq!(counters.bind_denied_total.load(Ordering::Relaxed), 0);
        assert_eq!(counters.dns_denied_total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_policy_enforcer_new() {
        let enforcer = PolicyEnforcer::new(make_policy());
        assert_eq!(enforcer.allowed_cidrs_parsed.len(), 0);
        assert_eq!(enforcer.denied_cidrs_parsed.len(), 0);
    }

    #[test]
    fn test_check_outbound_tcp_connect_allowed() {
        let policy = InstancePolicy {
            network: NetworkPolicy {
                allow_outbound_tcp: true,
                ..NetworkPolicy::default()
            },
            filesystem: FilesystemPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);
        let ip: IpAddr = "93.184.216.34".parse().unwrap();
        assert!(enforcer.check_outbound_tcp_connect(ip, 443).is_ok());
        // check_outbound_tcp_connect now atomically increments active counter
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::Relaxed),
            1
        );
        // Clean up
        enforcer.record_outbound_disconnect();
    }

    #[test]
    fn test_check_outbound_tcp_connect_denied_by_policy() {
        let policy = InstancePolicy {
            network: NetworkPolicy {
                allow_outbound_tcp: false,
                ..NetworkPolicy::default()
            },
            filesystem: FilesystemPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);
        let ip: IpAddr = "93.184.216.34".parse().unwrap();
        let result = enforcer.check_outbound_tcp_connect(ip, 443);
        assert!(result.is_err());
        match result.unwrap_err() {
            PolicyDenied::NetworkDisabled { protocol } => assert_eq!(protocol, "tcp"),
            _ => panic!("Expected NetworkDisabled"),
        }
    }

    #[test]
    fn test_check_outbound_tcp_connect_denied_by_cidr() {
        let policy = InstancePolicy {
            network: NetworkPolicy {
                allow_outbound_tcp: true,
                denied_cidrs: vec!["10.0.0.0/8".to_string()],
                ..NetworkPolicy::default()
            },
            filesystem: FilesystemPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);
        let ip: IpAddr = "10.1.2.3".parse().unwrap();
        let result = enforcer.check_outbound_tcp_connect(ip, 80);
        assert!(result.is_err());
        match result.unwrap_err() {
            PolicyDenied::DestinationDenied { reason, .. } => {
                assert!(reason.contains("denied_cidrs"));
            }
            _ => panic!("Expected DestinationDenied"),
        }
    }

    #[test]
    fn test_check_outbound_tcp_connect_allowed_cidr() {
        let policy = InstancePolicy {
            network: NetworkPolicy {
                allow_outbound_tcp: true,
                allowed_cidrs: vec!["93.184.216.0/24".to_string()],
                ..NetworkPolicy::default()
            },
            filesystem: FilesystemPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);
        let ip: IpAddr = "93.184.216.34".parse().unwrap();
        assert!(enforcer.check_outbound_tcp_connect(ip, 443).is_ok());
        // Clean up
        enforcer.record_outbound_disconnect();

        let denied_ip: IpAddr = "10.0.0.1".parse().unwrap();
        let result = enforcer.check_outbound_tcp_connect(denied_ip, 80);
        assert!(result.is_err());
    }

    #[test]
    fn test_check_outbound_tcp_connect_connection_limit() {
        let policy = InstancePolicy {
            network: NetworkPolicy {
                allow_outbound_tcp: true,
                max_outbound_connections: 2,
                ..NetworkPolicy::default()
            },
            filesystem: FilesystemPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);
        let ip: IpAddr = "93.184.216.34".parse().unwrap();

        // First two should succeed (and atomically increment)
        assert!(enforcer.check_outbound_tcp_connect(ip, 443).is_ok());
        assert!(enforcer.check_outbound_tcp_connect(ip, 443).is_ok());

        // Third should fail
        let result = enforcer.check_outbound_tcp_connect(ip, 443);
        assert!(result.is_err());
        match result.unwrap_err() {
            PolicyDenied::ConnectionLimitExceeded { current, limit } => {
                assert_eq!(current, 2);
                assert_eq!(limit, 2);
            }
            _ => panic!("Expected ConnectionLimitExceeded"),
        }

        // Disconnect one and try again
        enforcer.record_outbound_disconnect();
        assert!(enforcer.check_outbound_tcp_connect(ip, 443).is_ok());

        // Clean up
        enforcer.record_outbound_disconnect();
        enforcer.record_outbound_disconnect();
    }

    #[test]
    fn test_record_outbound_connect_and_disconnect() {
        let policy = InstancePolicy {
            network: NetworkPolicy {
                allow_outbound_tcp: true,
                ..NetworkPolicy::default()
            },
            filesystem: FilesystemPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);
        let ip: IpAddr = "93.184.216.34".parse().unwrap();

        // check_outbound_tcp_connect atomically increments active
        assert!(enforcer.check_outbound_tcp_connect(ip, 443).is_ok());
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::Relaxed),
            1
        );

        // record_outbound_connect only increments total
        enforcer.record_outbound_connect();
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::Relaxed),
            1
        );
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_total
                .load(Ordering::Relaxed),
            1
        );

        // Disconnect
        enforcer.record_outbound_disconnect();
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn test_record_outbound_disconnect_underflow_guard() {
        let enforcer = PolicyEnforcer::new(make_policy());
        // Calling disconnect when active is 0 should not underflow
        enforcer.record_outbound_disconnect();
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn test_check_egress_unlimited() {
        let policy = InstancePolicy {
            network: NetworkPolicy {
                max_egress_bytes: 0, // unlimited
                ..NetworkPolicy::default()
            },
            filesystem: FilesystemPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);
        assert!(enforcer.check_and_record_egress(1_000_000).is_ok());
        assert_eq!(
            enforcer.counters.egress_bytes.load(Ordering::Relaxed),
            1_000_000
        );
    }

    #[test]
    fn test_check_egress_limited() {
        let policy = InstancePolicy {
            network: NetworkPolicy {
                max_egress_bytes: 1000,
                ..NetworkPolicy::default()
            },
            filesystem: FilesystemPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);

        assert!(enforcer.check_and_record_egress(500).is_ok());
        assert_eq!(enforcer.counters.egress_bytes.load(Ordering::Relaxed), 500);

        assert!(enforcer.check_and_record_egress(500).is_ok());
        assert_eq!(enforcer.counters.egress_bytes.load(Ordering::Relaxed), 1000);

        let result = enforcer.check_and_record_egress(1);
        assert!(result.is_err());
        match result.unwrap_err() {
            PolicyDenied::EgressLimitExceeded {
                current,
                requested,
                limit,
            } => {
                assert_eq!(current, 1000);
                assert_eq!(requested, 1);
                assert_eq!(limit, 1000);
            }
            _ => panic!("Expected EgressLimitExceeded"),
        }
    }

    #[test]
    fn test_record_egress() {
        let enforcer = PolicyEnforcer::new(make_policy());
        enforcer
            .counters
            .egress_bytes
            .fetch_add(42, Ordering::Relaxed);
        assert_eq!(enforcer.counters.egress_bytes.load(Ordering::Relaxed), 42);
    }

    #[test]
    fn test_check_dns_lookup() {
        let policy = InstancePolicy {
            network: NetworkPolicy {
                allow_dns: true,
                ..NetworkPolicy::default()
            },
            filesystem: FilesystemPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);
        assert!(enforcer.check_dns_lookup().is_ok());

        let policy_denied = InstancePolicy {
            network: NetworkPolicy {
                allow_dns: false,
                ..NetworkPolicy::default()
            },
            filesystem: FilesystemPolicy::default(),
        };
        let enforcer_denied = PolicyEnforcer::new(policy_denied);
        assert!(enforcer_denied.check_dns_lookup().is_err());
    }

    #[test]
    fn test_check_bind() {
        let policy = InstancePolicy {
            network: NetworkPolicy {
                allowed_bind_ports: vec![8080, 9090],
                ..NetworkPolicy::default()
            },
            filesystem: FilesystemPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);
        assert!(enforcer.check_bind(8080).is_ok());
        assert!(enforcer.check_bind(9090).is_ok());
        assert!(enforcer.check_bind(3000).is_err());
    }

    #[test]
    fn test_check_fd_open() {
        let policy = InstancePolicy {
            filesystem: FilesystemPolicy {
                max_open_fds: 2,
                ..FilesystemPolicy::default()
            },
            network: NetworkPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);

        assert!(enforcer.check_fd_open().is_ok());
        assert!(enforcer.check_fd_open().is_ok());

        let result = enforcer.check_fd_open();
        assert!(result.is_err());
        match result.unwrap_err() {
            PolicyDenied::FdLimitExceeded { current, limit } => {
                assert_eq!(current, 2);
                assert_eq!(limit, 2);
            }
            _ => panic!("Expected FdLimitExceeded"),
        }

        // Close one and try again
        enforcer.record_fd_close();
        assert!(enforcer.check_fd_open().is_ok());

        // Clean up
        enforcer.record_fd_close();
        enforcer.record_fd_close();
    }

    #[test]
    fn test_record_fd_open_and_close() {
        let enforcer = PolicyEnforcer::new(make_policy());

        // check_fd_open atomically increments open_fds
        assert!(enforcer.check_fd_open().is_ok());
        assert_eq!(enforcer.counters.open_fds.load(Ordering::Relaxed), 1);

        // record_fd_open only increments total
        enforcer.record_fd_open();
        assert_eq!(enforcer.counters.open_fds.load(Ordering::Relaxed), 1);
        assert_eq!(enforcer.counters.fd_open_total.load(Ordering::Relaxed), 1);

        // Close
        enforcer.record_fd_close();
        assert_eq!(enforcer.counters.open_fds.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_record_fd_close_underflow_guard() {
        let enforcer = PolicyEnforcer::new(make_policy());
        // Calling close when open_fds is 0 should not underflow
        enforcer.record_fd_close();
        assert_eq!(enforcer.counters.open_fds.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_check_fs_write() {
        let policy = InstancePolicy {
            filesystem: FilesystemPolicy {
                max_fs_write_bytes: 1000,
                ..FilesystemPolicy::default()
            },
            network: NetworkPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);

        assert!(enforcer.check_and_record_fs_write(500).is_ok());
        assert_eq!(
            enforcer.counters.fs_write_bytes.load(Ordering::Relaxed),
            500
        );

        assert!(enforcer.check_and_record_fs_write(500).is_ok());
        assert_eq!(
            enforcer.counters.fs_write_bytes.load(Ordering::Relaxed),
            1000
        );

        let result = enforcer.check_and_record_fs_write(1);
        assert!(result.is_err());
        match result.unwrap_err() {
            PolicyDenied::FsWriteLimitExceeded {
                current,
                requested,
                limit,
            } => {
                assert_eq!(current, 1000);
                assert_eq!(requested, 1);
                assert_eq!(limit, 1000);
            }
            _ => panic!("Expected FsWriteLimitExceeded"),
        }
    }

    #[test]
    fn test_record_fs_write() {
        let enforcer = PolicyEnforcer::new(make_policy());
        enforcer
            .counters
            .fs_write_bytes
            .fetch_add(42, Ordering::Relaxed);
        assert_eq!(enforcer.counters.fs_write_bytes.load(Ordering::Relaxed), 42);
    }

    #[test]
    fn test_check_file_create() {
        let policy = InstancePolicy {
            filesystem: FilesystemPolicy {
                allow_file_create: true,
                ..FilesystemPolicy::default()
            },
            network: NetworkPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);
        assert!(enforcer.check_file_create().is_ok());

        let policy_denied = InstancePolicy {
            filesystem: FilesystemPolicy {
                allow_file_create: false,
                ..FilesystemPolicy::default()
            },
            network: NetworkPolicy::default(),
        };
        let enforcer_denied = PolicyEnforcer::new(policy_denied);
        assert!(enforcer_denied.check_file_create().is_err());
    }

    #[test]
    fn test_check_file_delete() {
        let policy = InstancePolicy {
            filesystem: FilesystemPolicy {
                allow_file_delete: true,
                ..FilesystemPolicy::default()
            },
            network: NetworkPolicy::default(),
        };
        let enforcer = PolicyEnforcer::new(policy);
        assert!(enforcer.check_file_delete().is_ok());

        let policy_denied = InstancePolicy {
            filesystem: FilesystemPolicy {
                allow_file_delete: false,
                ..FilesystemPolicy::default()
            },
            network: NetworkPolicy::default(),
        };
        let enforcer_denied = PolicyEnforcer::new(policy_denied);
        assert!(enforcer_denied.check_file_delete().is_err());
    }

    #[test]
    fn test_ip_in_cidrs() {
        let cidrs: Vec<ipnet::IpNet> = vec![
            "10.0.0.0/8".parse().unwrap(),
            "192.168.0.0/16".parse().unwrap(),
        ];
        let ip_in: IpAddr = "10.1.2.3".parse().unwrap();
        let ip_in2: IpAddr = "192.168.1.1".parse().unwrap();
        let ip_out: IpAddr = "93.184.216.34".parse().unwrap();

        assert!(PolicyEnforcer::ip_in_cidrs(ip_in, &cidrs));
        assert!(PolicyEnforcer::ip_in_cidrs(ip_in2, &cidrs));
        assert!(!PolicyEnforcer::ip_in_cidrs(ip_out, &cidrs));
    }

    #[test]
    fn test_parse_cidrs_invalid_skipped() {
        let cidrs = vec![
            "10.0.0.0/8".to_string(),
            "not-a-cidr".to_string(),
            "192.168.0.0/16".to_string(),
        ];
        let parsed = PolicyEnforcer::parse_cidrs(&cidrs);
        assert_eq!(parsed.len(), 2);
    }
}

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
}

impl PolicyEnforcer {
    pub fn new(policy: InstancePolicy) -> Self {
        PolicyEnforcer {
            policy,
            counters: Arc::new(PolicyCounters::new()),
        }
    }

    // ── Network Policy Checks ──────────────────────────────────────

    /// Check if an outbound TCP connection is allowed.
    /// Returns Ok(()) if allowed, Err with reason if denied.
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
        if self.ip_in_cidrs(dest_ip, &self.policy.network.denied_cidrs) {
            self.counters
                .connection_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::DestinationDenied {
                ip: dest_ip.to_string(),
                reason: "destination in denied_cidrs".to_string(),
            });
        }

        // Check allowed CIDRs (if non-empty, only these are allowed)
        if !self.policy.network.allowed_cidrs.is_empty()
            && !self.ip_in_cidrs(dest_ip, &self.policy.network.allowed_cidrs)
        {
            self.counters
                .connection_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::DestinationDenied {
                ip: dest_ip.to_string(),
                reason: "destination not in allowed_cidrs".to_string(),
            });
        }

        // Check connection count
        let current = self
            .counters
            .outbound_connections_active
            .load(Ordering::Relaxed);
        if current >= self.policy.network.max_outbound_connections {
            self.counters
                .connection_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::ConnectionLimitExceeded {
                current,
                limit: self.policy.network.max_outbound_connections,
            });
        }

        Ok(())
    }

    /// Record that an outbound connection was established.
    pub fn record_outbound_connect(&self) {
        self.counters
            .outbound_connections_active
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .outbound_connections_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record that an outbound connection was closed.
    pub fn record_outbound_disconnect(&self) {
        self.counters
            .outbound_connections_active
            .fetch_sub(1, Ordering::Relaxed);
    }

    /// Check if egress data is allowed (before sending).
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
    pub fn record_egress(&self, bytes: u64) {
        self.counters
            .egress_bytes
            .fetch_add(bytes, Ordering::Relaxed);
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

    /// Check if opening a file descriptor is allowed.
    pub fn check_fd_open(&self) -> Result<(), PolicyDenied> {
        let current = self.counters.open_fds.load(Ordering::Relaxed);
        if current >= self.policy.filesystem.max_open_fds {
            self.counters
                .fd_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::FdLimitExceeded {
                current,
                limit: self.policy.filesystem.max_open_fds,
            });
        }
        Ok(())
    }

    /// Record that an FD was opened.
    pub fn record_fd_open(&self) {
        self.counters.open_fds.fetch_add(1, Ordering::Relaxed);
        self.counters.fd_open_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that an FD was closed.
    pub fn record_fd_close(&self) {
        self.counters.open_fds.fetch_sub(1, Ordering::Relaxed);
    }

    /// Check if a filesystem write is allowed.
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
    pub fn record_fs_write(&self, bytes: u64) {
        self.counters
            .fs_write_bytes
            .fetch_add(bytes, Ordering::Relaxed);
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

    /// Check if an IP address falls within any of the given CIDR strings.
    fn ip_in_cidrs(&self, ip: IpAddr, cidrs: &[String]) -> bool {
        for cidr_str in cidrs {
            if let Ok(cidr) = cidr_str.parse::<ipnet::IpNet>() {
                if cidr.contains(&ip) {
                    return true;
                }
            }
        }
        false
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
    use std::net::IpAddr;

    #[test]
    fn test_policy_counters_new() {
        let counters = PolicyCounters::new();
        assert_eq!(
            counters.outbound_connections_active.load(Ordering::SeqCst),
            0
        );
        assert_eq!(
            counters.outbound_connections_total.load(Ordering::SeqCst),
            0
        );
        assert_eq!(counters.egress_bytes.load(Ordering::SeqCst), 0);
        assert_eq!(counters.dns_lookups_total.load(Ordering::SeqCst), 0);
        assert_eq!(
            counters.inbound_connections_active.load(Ordering::SeqCst),
            0
        );
        assert_eq!(counters.open_fds.load(Ordering::SeqCst), 0);
        assert_eq!(counters.fd_open_total.load(Ordering::SeqCst), 0);
        assert_eq!(counters.fs_write_bytes.load(Ordering::SeqCst), 0);
        assert_eq!(counters.fs_read_bytes.load(Ordering::SeqCst), 0);
        assert_eq!(counters.file_creates_total.load(Ordering::SeqCst), 0);
        assert_eq!(counters.file_deletes_total.load(Ordering::SeqCst), 0);
        assert_eq!(counters.connection_denied_total.load(Ordering::SeqCst), 0);
        assert_eq!(counters.egress_denied_total.load(Ordering::SeqCst), 0);
        assert_eq!(counters.fd_denied_total.load(Ordering::SeqCst), 0);
        assert_eq!(counters.fs_write_denied_total.load(Ordering::SeqCst), 0);
        assert_eq!(counters.bind_denied_total.load(Ordering::SeqCst), 0);
        assert_eq!(counters.dns_denied_total.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_policy_enforcer_new() {
        let policy = InstancePolicy::default();
        let enforcer = PolicyEnforcer::new(policy);
        // Check that the policy is stored correctly
        assert!(enforcer.policy.network.allow_outbound_tcp);
        assert!(!enforcer.policy.network.allow_outbound_udp);
        assert!(enforcer.policy.network.allow_dns);
        assert_eq!(enforcer.policy.network.max_outbound_connections, 100);
        assert_eq!(enforcer.policy.filesystem.max_open_fds, 64);
    }

    #[test]
    fn test_check_outbound_tcp_connect_allowed() {
        let policy = InstancePolicy::default();
        let enforcer = PolicyEnforcer::new(policy);
        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        let result = enforcer.check_outbound_tcp_connect(ip, 80);
        assert!(result.is_ok());
        // Check that the connection count is not incremented until we record
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::SeqCst),
            0
        );
    }

    #[test]
    fn test_check_outbound_tcp_connect_denied_by_policy() {
        let mut policy = InstancePolicy::default();
        policy.network.allow_outbound_tcp = false;
        let enforcer = PolicyEnforcer::new(policy);
        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        let result = enforcer.check_outbound_tcp_connect(ip, 80);
        assert!(result.is_err());
        if let Err(PolicyDenied::NetworkDisabled { protocol }) = result {
            assert_eq!(protocol, "tcp");
        } else {
            panic!("Expected NetworkDisabled error");
        }
        // Check that the denied counter was incremented
        assert_eq!(
            enforcer
                .counters
                .connection_denied_total
                .load(Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn test_check_outbound_tcp_connect_denied_by_cidr() {
        let mut policy = InstancePolicy::default();
        policy.network.denied_cidrs = vec!["192.168.1.0/24".to_string()];
        let enforcer = PolicyEnforcer::new(policy);
        let ip: IpAddr = "192.168.1.1".parse().unwrap();
        let result = enforcer.check_outbound_tcp_connect(ip, 80);
        assert!(result.is_err());
        if let Err(PolicyDenied::DestinationDenied { ip: ip_str, reason }) = result {
            assert_eq!(ip_str, "192.168.1.1");
            assert_eq!(reason, "destination in denied_cidrs");
        } else {
            panic!("Expected DestinationDenied error");
        }
        assert_eq!(
            enforcer
                .counters
                .connection_denied_total
                .load(Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn test_check_outbound_tcp_connect_allowed_cidr() {
        let mut policy = InstancePolicy::default();
        policy.network.allowed_cidrs = vec!["10.0.0.0/8".to_string()];
        let enforcer = PolicyEnforcer::new(policy);
        // IP in allowed CIDR should pass
        let ip1: IpAddr = "10.1.2.3".parse().unwrap();
        assert!(enforcer.check_outbound_tcp_connect(ip1, 80).is_ok());
        // IP not in allowed CIDR should fail
        let ip2: IpAddr = "192.168.1.1".parse().unwrap();
        let result = enforcer.check_outbound_tcp_connect(ip2, 80);
        assert!(result.is_err());
        if let Err(PolicyDenied::DestinationDenied { ip: ip_str, reason }) = result {
            assert_eq!(ip_str, "192.168.1.1");
            assert_eq!(reason, "destination not in allowed_cidrs");
        } else {
            panic!("Expected DestinationDenied error");
        }
    }

    #[test]
    fn test_check_outbound_tcp_connect_connection_limit() {
        let mut policy = InstancePolicy::default();
        policy.network.max_outbound_connections = 2;
        let enforcer = PolicyEnforcer::new(policy);
        let ip: IpAddr = "192.168.1.1".parse().unwrap();

        // Simulate two active connections
        enforcer
            .counters
            .outbound_connections_active
            .store(2, Ordering::SeqCst);

        let result = enforcer.check_outbound_tcp_connect(ip, 80);
        assert!(result.is_err());
        if let Err(PolicyDenied::ConnectionLimitExceeded { current, limit }) = result {
            assert_eq!(current, 2);
            assert_eq!(limit, 2);
        } else {
            panic!("Expected ConnectionLimitExceeded error");
        }
        assert_eq!(
            enforcer
                .counters
                .connection_denied_total
                .load(Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn test_record_outbound_connect_and_disconnect() {
        let policy = InstancePolicy::default();
        let enforcer = PolicyEnforcer::new(policy);
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::SeqCst),
            0
        );
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_total
                .load(Ordering::SeqCst),
            0
        );

        enforcer.record_outbound_connect();
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::SeqCst),
            1
        );
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_total
                .load(Ordering::SeqCst),
            1
        );

        enforcer.record_outbound_disconnect();
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_active
                .load(Ordering::SeqCst),
            0
        );
        // Total should remain 1
        assert_eq!(
            enforcer
                .counters
                .outbound_connections_total
                .load(Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn test_check_egress_unlimited() {
        let policy = InstancePolicy::default();
        let enforcer = PolicyEnforcer::new(policy);
        // Default max_egress_bytes is 0 (unlimited)
        assert!(enforcer.check_egress(1000).is_ok());
        assert_eq!(enforcer.counters.egress_bytes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_check_egress_limited() {
        let mut policy = InstancePolicy::default();
        policy.network.max_egress_bytes = 500;
        let enforcer = PolicyEnforcer::new(policy);
        // Set current egress to 300
        enforcer.counters.egress_bytes.store(300, Ordering::SeqCst);

        // 300 + 200 = 500, should be ok
        assert!(enforcer.check_egress(200).is_ok());
        // 300 + 201 = 501, should fail
        let result = enforcer.check_egress(201);
        assert!(result.is_err());
        if let Err(PolicyDenied::EgressLimitExceeded {
            current,
            requested,
            limit,
        }) = result
        {
            assert_eq!(current, 300);
            assert_eq!(requested, 201);
            assert_eq!(limit, 500);
        } else {
            panic!("Expected EgressLimitExceeded error");
        }
        assert_eq!(
            enforcer.counters.egress_denied_total.load(Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn test_record_egress() {
        let policy = InstancePolicy::default();
        let enforcer = PolicyEnforcer::new(policy);
        enforcer.record_egress(100);
        assert_eq!(enforcer.counters.egress_bytes.load(Ordering::SeqCst), 100);
        enforcer.record_egress(50);
        assert_eq!(enforcer.counters.egress_bytes.load(Ordering::SeqCst), 150);
    }

    #[test]
    fn test_check_dns_lookup() {
        let policy = InstancePolicy::default();
        let enforcer = PolicyEnforcer::new(policy);
        assert!(enforcer.check_dns_lookup().is_ok());
        assert_eq!(
            enforcer.counters.dns_lookups_total.load(Ordering::SeqCst),
            1
        );

        // Now test with DNS disabled
        let mut policy = InstancePolicy::default();
        policy.network.allow_dns = false;
        let enforcer = PolicyEnforcer::new(policy);
        let result = enforcer.check_dns_lookup();
        assert!(result.is_err());
        assert!(matches!(result, Err(PolicyDenied::DnsDisabled)));
        assert_eq!(enforcer.counters.dns_denied_total.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_check_bind() {
        let mut policy = InstancePolicy::default();
        policy.network.allowed_bind_ports = vec![8080, 9090];
        let enforcer = PolicyEnforcer::new(policy);
        assert!(enforcer.check_bind(8080).is_ok());
        assert!(enforcer.check_bind(9090).is_ok());
        let result = enforcer.check_bind(3000);
        assert!(result.is_err());
        if let Err(PolicyDenied::BindDenied { port, allowed }) = result {
            assert_eq!(port, 3000);
            assert_eq!(allowed, vec![8080, 9090]);
        } else {
            panic!("Expected BindDenied error");
        }
        assert_eq!(
            enforcer.counters.bind_denied_total.load(Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn test_check_fd_open() {
        let mut policy = InstancePolicy::default();
        policy.filesystem.max_open_fds = 2;
        let enforcer = PolicyEnforcer::new(policy);
        // Set current open FDs to 2
        enforcer.counters.open_fds.store(2, Ordering::SeqCst);
        let result = enforcer.check_fd_open();
        assert!(result.is_err());
        if let Err(PolicyDenied::FdLimitExceeded { current, limit }) = result {
            assert_eq!(current, 2);
            assert_eq!(limit, 2);
        } else {
            panic!("Expected FdLimitExceeded error");
        }
        assert_eq!(enforcer.counters.fd_denied_total.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_record_fd_open_and_close() {
        let policy = InstancePolicy::default();
        let enforcer = PolicyEnforcer::new(policy);
        assert_eq!(enforcer.counters.open_fds.load(Ordering::SeqCst), 0);
        assert_eq!(enforcer.counters.fd_open_total.load(Ordering::SeqCst), 0);

        enforcer.record_fd_open();
        assert_eq!(enforcer.counters.open_fds.load(Ordering::SeqCst), 1);
        assert_eq!(enforcer.counters.fd_open_total.load(Ordering::SeqCst), 1);

        enforcer.record_fd_close();
        assert_eq!(enforcer.counters.open_fds.load(Ordering::SeqCst), 0);
        assert_eq!(enforcer.counters.fd_open_total.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_check_fs_write() {
        let mut policy = InstancePolicy::default();
        policy.filesystem.max_fs_write_bytes = 1000;
        let enforcer = PolicyEnforcer::new(policy);
        // Set current write to 600
        enforcer
            .counters
            .fs_write_bytes
            .store(600, Ordering::SeqCst);

        // 600 + 400 = 1000, ok
        assert!(enforcer.check_fs_write(400).is_ok());
        // 600 + 401 = 1001, fail
        let result = enforcer.check_fs_write(401);
        assert!(result.is_err());
        if let Err(PolicyDenied::FsWriteLimitExceeded {
            current,
            requested,
            limit,
        }) = result
        {
            assert_eq!(current, 600);
            assert_eq!(requested, 401);
            assert_eq!(limit, 1000);
        } else {
            panic!("Expected FsWriteLimitExceeded error");
        }
        assert_eq!(
            enforcer
                .counters
                .fs_write_denied_total
                .load(Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn test_record_fs_write() {
        let policy = InstancePolicy::default();
        let enforcer = PolicyEnforcer::new(policy);
        enforcer.record_fs_write(100);
        assert_eq!(enforcer.counters.fs_write_bytes.load(Ordering::SeqCst), 100);
        enforcer.record_fs_write(200);
        assert_eq!(enforcer.counters.fs_write_bytes.load(Ordering::SeqCst), 300);
    }

    #[test]
    fn test_check_file_create() {
        let policy = InstancePolicy::default();
        let enforcer = PolicyEnforcer::new(policy);
        // Default is false
        let result = enforcer.check_file_create();
        assert!(result.is_err());
        assert!(matches!(result, Err(PolicyDenied::FileCreateDenied)));

        // Now with allow_file_create = true
        let mut policy = InstancePolicy::default();
        policy.filesystem.allow_file_create = true;
        let enforcer = PolicyEnforcer::new(policy);
        assert!(enforcer.check_file_create().is_ok());
        assert_eq!(
            enforcer.counters.file_creates_total.load(Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn test_check_file_delete() {
        let policy = InstancePolicy::default();
        let enforcer = PolicyEnforcer::new(policy);
        // Default is false
        let result = enforcer.check_file_delete();
        assert!(result.is_err());
        assert!(matches!(result, Err(PolicyDenied::FileDeleteDenied)));

        // Now with allow_file_delete = true
        let mut policy = InstancePolicy::default();
        policy.filesystem.allow_file_delete = true;
        let enforcer = PolicyEnforcer::new(policy);
        assert!(enforcer.check_file_delete().is_ok());
        assert_eq!(
            enforcer.counters.file_deletes_total.load(Ordering::SeqCst),
            1
        );
    }

    #[test]
    fn test_ip_in_cidrs() {
        let policy = InstancePolicy::default();
        let enforcer = PolicyEnforcer::new(policy);
        let ip: IpAddr = "10.1.2.3".parse().unwrap();
        let cidrs = vec!["10.0.0.0/8".to_string(), "192.168.0.0/16".to_string()];
        assert!(enforcer.ip_in_cidrs(ip, &cidrs));
        let ip2: IpAddr = "172.16.1.1".parse().unwrap();
        assert!(!enforcer.ip_in_cidrs(ip2, &cidrs));
    }
}

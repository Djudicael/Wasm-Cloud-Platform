use std::net::IpAddr;
use std::sync::atomic::Ordering;

use super::{PolicyDenied, PolicyEnforcer};

impl PolicyEnforcer {
    /// Check if an outbound TCP connection is allowed and atomically reserve a slot.
    pub fn check_outbound_tcp_connect(
        &self,
        dest_ip: IpAddr,
        _dest_port: u16,
    ) -> Result<(), PolicyDenied> {
        self.check_outbound_connect(dest_ip, _dest_port, false)
    }

    pub(crate) fn check_outbound_http_connect(
        &self,
        ip: IpAddr,
        port: u16,
    ) -> Result<(), PolicyDenied> {
        self.check_outbound_connect(ip, port, true)
    }

    fn check_outbound_connect(
        &self,
        dest_ip: IpAddr,
        port: u16,
        http: bool,
    ) -> Result<(), PolicyDenied> {
        let dest_ip = dest_ip.to_canonical();
        if let Err(reason) = self
            .policy
            .network
            .check_profile_destination(std::net::SocketAddr::new(dest_ip, port), http)
        {
            self.counters
                .connection_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::DestinationDenied {
                ip: dest_ip.to_string(),
                reason: reason.into(),
            });
        }
        if !self.policy.network.allow_outbound_tcp {
            self.counters
                .connection_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::NetworkDisabled { protocol: "tcp" });
        }

        if Self::ip_in_cidrs(dest_ip, &self.denied_cidrs_parsed) {
            self.counters
                .connection_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::DestinationDenied {
                ip: dest_ip.to_string(),
                reason: "destination in denied_cidrs".to_string(),
            });
        }

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
                self.local_outbound_connections_active
                    .fetch_add(1, Ordering::AcqRel);
                break;
            }
        }

        Ok(())
    }

    /// Record that an outbound connection was established.
    pub fn record_outbound_connect(&self) {
        self.counters
            .outbound_connections_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record that an outbound connection was closed.
    pub fn record_outbound_disconnect(&self) {
        // Do not publish a transient u32::MAX when a duplicate close races
        // another release or store teardown.
        let released = self.local_outbound_connections_active.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |current| current.checked_sub(1),
        );
        if released.is_err() {
            return;
        }

        let _ = self.counters.outbound_connections_active.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |current| current.checked_sub(1),
        );
    }

    /// Check if egress data is allowed (before sending).
    #[deprecated(
        since = "0.2.0",
        note = "Use check_and_record_egress instead to avoid TOCTOU races"
    )]
    pub fn check_egress(&self, additional_bytes: u64) -> Result<(), PolicyDenied> {
        if self.policy.network.max_egress_bytes == 0 {
            return Ok(());
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
    pub fn check_and_record_egress(&self, bytes: u64) -> Result<(), PolicyDenied> {
        let limit = self.policy.network.max_egress_bytes;
        if limit == 0 {
            self.counters
                .egress_bytes
                .fetch_add(bytes, Ordering::Relaxed);
            return Ok(());
        }

        loop {
            let current = self.counters.egress_bytes.load(Ordering::Acquire);
            if bytes > limit.saturating_sub(current) {
                self.counters
                    .egress_denied_total
                    .fetch_add(1, Ordering::Relaxed);
                return Err(PolicyDenied::EgressLimitExceeded {
                    current,
                    requested: bytes,
                    limit,
                });
            }
            let new_val = current + bytes;
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

    pub fn check_tcp_bind(&self, port: u16) -> Result<(), PolicyDenied> {
        if !self.policy.network.allow_inbound {
            self.counters
                .bind_denied_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(PolicyDenied::NetworkDisabled {
                protocol: "tcp_bind",
            });
        }
        self.check_bind(port)
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
}

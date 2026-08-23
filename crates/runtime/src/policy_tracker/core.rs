use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use common::policy::InstancePolicy;

use super::PolicyCounters;

/// The policy enforcement engine. Lives in StoreState and is called by custom
/// WASI host functions before delegating to the real implementation.
#[derive(Clone)]
pub struct PolicyEnforcer {
    pub policy: InstancePolicy,
    pub counters: Arc<PolicyCounters>,
    /// Connections reserved by this store. The public counters may be shared by
    /// many short-lived WASI HTTP stores, so store teardown must release only
    /// the reservations owned by that store.
    pub(super) local_outbound_connections_active: Arc<AtomicU32>,
    /// Pre-parsed allowed CIDRs, parsed once at construction.
    pub(super) allowed_cidrs_parsed: Vec<ipnet::IpNet>,
    /// Pre-parsed denied CIDRs, parsed once at construction.
    pub(super) denied_cidrs_parsed: Vec<ipnet::IpNet>,
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
        Self::with_counters(policy, Arc::new(PolicyCounters::new()))
    }

    pub fn with_counters(policy: InstancePolicy, counters: Arc<PolicyCounters>) -> Self {
        let allowed_cidrs_parsed = Self::parse_cidrs(&policy.network.allowed_cidrs);
        let denied_cidrs_parsed = Self::parse_cidrs(&policy.network.denied_cidrs);
        PolicyEnforcer {
            policy,
            counters,
            local_outbound_connections_active: Arc::new(AtomicU32::new(0)),
            allowed_cidrs_parsed,
            denied_cidrs_parsed,
        }
    }

    /// Parse CIDR strings into `IpNet` values, logging warnings for invalid entries.
    pub(super) fn parse_cidrs(cidrs: &[String]) -> Vec<ipnet::IpNet> {
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

    pub(crate) fn update_peak_u64(peak: &AtomicU64, candidate: u64) {
        loop {
            let current_peak = peak.load(Ordering::Acquire);
            if candidate <= current_peak {
                return;
            }
            if peak
                .compare_exchange(current_peak, candidate, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    pub(crate) fn update_peak_u32(peak: &AtomicU32, candidate: u32) {
        loop {
            let current_peak = peak.load(Ordering::Acquire);
            if candidate <= current_peak {
                return;
            }
            if peak
                .compare_exchange(current_peak, candidate, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    pub fn reset_active_counters(&self) {
        self.local_outbound_connections_active
            .store(0, Ordering::Release);
        self.counters
            .outbound_connections_active
            .store(0, Ordering::Release);
        self.counters
            .inbound_connections_active
            .store(0, Ordering::Release);
        self.counters.open_fds.store(0, Ordering::Release);
    }

    /// Release outbound reservations owned by this store without disturbing
    /// concurrent stores that share the aggregate policy counters.
    pub(crate) fn release_tracked_outbound_connections(&self) {
        let owned = self
            .local_outbound_connections_active
            .swap(0, Ordering::AcqRel);
        if owned == 0 {
            return;
        }
        let _ = self.counters.outbound_connections_active.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |current| Some(current.saturating_sub(owned)),
        );
    }

    /// Check if an IP address falls within any of the given pre-parsed CIDRs.
    pub(super) fn ip_in_cidrs(ip: IpAddr, cidrs: &[ipnet::IpNet]) -> bool {
        cidrs.iter().any(|cidr| cidr.contains(&ip))
    }

    pub(super) fn update_peak(peak: &AtomicU32, candidate: u32) {
        Self::update_peak_u32(peak, candidate);
    }
}

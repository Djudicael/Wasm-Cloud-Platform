use crate::policy_tracker::PolicyEnforcer;
use common::policy::InstancePolicy;
use std::collections::HashSet;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

/// Simplified mirror of `wasmtime_wasi::sockets::SocketAddrUse` for the public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketAddrUse {
    TcpBind,
    TcpListen,
    TcpAccept,
    TcpConnect,
    UdpBind,
    UdpSend,
    UdpReceive,
}

/// Async callback for validating outbound socket addresses.
pub type SocketAddrCheckFn = Arc<
    dyn Fn(SocketAddr, SocketAddrUse) -> Pin<Box<dyn Future<Output = bool> + Send + Sync>>
        + Send
        + Sync,
>;

/// Snapshot of the instance network policy used by the WASI socket address checker.
#[derive(Debug, Clone)]
pub(crate) struct SocketPolicyCheck {
    network: common::policy::NetworkPolicy,
    allow_inbound: bool,
    allow_outbound_tcp: bool,
    allow_outbound_udp: bool,
    allowed_bind_ports: Arc<HashSet<u16>>,
    allowed_cidrs: Arc<Vec<ipnet::IpNet>>,
    denied_cidrs: Arc<Vec<ipnet::IpNet>>,
}

impl SocketPolicyCheck {
    pub(crate) fn from_instance_policy(policy: &InstancePolicy) -> Self {
        SocketPolicyCheck {
            network: policy.network.clone(),
            allow_inbound: policy.network.allow_inbound,
            allow_outbound_tcp: policy.network.allow_outbound_tcp,
            allow_outbound_udp: policy.network.allow_outbound_udp,
            allowed_bind_ports: Arc::new(
                policy.network.allowed_bind_ports.iter().copied().collect(),
            ),
            allowed_cidrs: Arc::new(Self::parse_cidrs(&policy.network.allowed_cidrs)),
            denied_cidrs: Arc::new(Self::parse_cidrs(&policy.network.denied_cidrs)),
        }
    }

    fn parse_cidrs(cidrs: &[String]) -> Vec<ipnet::IpNet> {
        cidrs
            .iter()
            .filter_map(|cidr| match cidr.parse::<ipnet::IpNet>() {
                Ok(net) => Some(net),
                Err(err) => {
                    tracing::warn!(cidr, error = %err, "ignoring invalid CIDR in socket policy snapshot");
                    None
                }
            })
            .collect()
    }

    fn outbound_ip_allowed(&self, ip: IpAddr) -> Result<(), &'static str> {
        let ip = ip.to_canonical();
        if self.denied_cidrs.iter().any(|cidr| cidr.contains(&ip)) {
            return Err("destination in denied_cidrs");
        }

        if !self.allowed_cidrs.is_empty()
            && !self.allowed_cidrs.iter().any(|cidr| cidr.contains(&ip))
        {
            return Err("destination not in allowed_cidrs");
        }

        Ok(())
    }

    pub(crate) fn check(
        &self,
        addr: SocketAddr,
        use_type: SocketAddrUse,
    ) -> Result<(), &'static str> {
        match use_type {
            // WASI TCP clients bind an ephemeral local endpoint before connect.
            // Listen is checked separately against the assigned service port.
            SocketAddrUse::TcpBind if addr.port() == 0 && self.allow_outbound_tcp => Ok(()),
            SocketAddrUse::TcpBind | SocketAddrUse::TcpListen => {
                if !self.allow_inbound {
                    return Err("inbound tcp bind disabled");
                }
                if !self.allowed_bind_ports.contains(&addr.port()) {
                    return Err("bind port not allowed");
                }
                Ok(())
            }
            SocketAddrUse::TcpAccept => {
                if !self.allow_inbound {
                    return Err("inbound tcp accept disabled");
                }
                Ok(())
            }
            SocketAddrUse::TcpConnect => {
                self.network.check_profile_destination(addr, false)?;
                if !self.allow_outbound_tcp {
                    return Err("outbound tcp disabled");
                }
                self.outbound_ip_allowed(addr.ip())
            }
            SocketAddrUse::UdpBind | SocketAddrUse::UdpSend | SocketAddrUse::UdpReceive => {
                if !self.allow_outbound_udp {
                    return Err("outbound udp disabled");
                }
                self.outbound_ip_allowed(addr.ip())
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn compose_socket_addr_check(
    policy_check: SocketPolicyCheck,
    policy_enforcer: PolicyEnforcer,
    extra_check: Option<SocketAddrCheckFn>,
) -> SocketAddrCheckFn {
    compose_addr_check(policy_check, policy_enforcer, extra_check, false, false)
}

pub(super) fn compose_resource_socket_addr_check(
    policy_check: SocketPolicyCheck,
    policy_enforcer: PolicyEnforcer,
    extra_check: Option<SocketAddrCheckFn>,
) -> SocketAddrCheckFn {
    compose_addr_check(policy_check, policy_enforcer, extra_check, false, true)
}

pub(super) fn compose_http_addr_check(
    policy_check: SocketPolicyCheck,
    policy_enforcer: PolicyEnforcer,
    extra_check: Option<SocketAddrCheckFn>,
) -> SocketAddrCheckFn {
    compose_addr_check(policy_check, policy_enforcer, extra_check, true, false)
}

fn compose_addr_check(
    policy_check: SocketPolicyCheck,
    policy_enforcer: PolicyEnforcer,
    extra_check: Option<SocketAddrCheckFn>,
    http: bool,
    resource_managed: bool,
) -> SocketAddrCheckFn {
    Arc::new(move |addr, use_type| {
        // The supervisor must see mapped IPv4 as IPv4 too, otherwise its
        // loopback/namespace gate could mistake it for an external address.
        let addr = SocketAddr::new(addr.ip().to_canonical(), addr.port());
        let policy_enforcer = policy_enforcer.clone();
        let snapshot_check = policy_check.clone();
        let extra_check = extra_check.clone();
        Box::pin(async move {
            let reserved_outbound_slot = match use_type {
                SocketAddrUse::TcpBind if addr.port() == 0 && snapshot_check.allow_outbound_tcp => {
                    false
                }
                SocketAddrUse::TcpBind | SocketAddrUse::TcpListen => {
                    match policy_enforcer.check_tcp_bind(addr.port()) {
                        Ok(()) => false,
                        Err(err) => {
                            tracing::warn!(
                                dest = %addr,
                                use_type = ?use_type,
                                error = ?err,
                                "socket operation denied by runtime policy"
                            );
                            return false;
                        }
                    }
                }
                SocketAddrUse::TcpAccept => match snapshot_check.check(addr, use_type) {
                    Ok(()) => false,
                    Err(reason) => {
                        tracing::warn!(
                            dest = %addr,
                            use_type = ?use_type,
                            reason,
                            "socket operation denied by runtime policy"
                        );
                        return false;
                    }
                },
                SocketAddrUse::TcpConnect if resource_managed => {
                    // The P2 start-connect host call owns the reservation.
                    // Keep address/protocol checks and the supervisor gate here.
                    if snapshot_check.check(addr, use_type).is_err() {
                        return false;
                    }
                    false
                }
                SocketAddrUse::TcpConnect => {
                    let decision = if http {
                        policy_enforcer.check_outbound_http_connect(addr.ip(), addr.port())
                    } else {
                        policy_enforcer.check_outbound_tcp_connect(addr.ip(), addr.port())
                    };
                    match decision {
                        Ok(()) => true,
                        Err(err) => {
                            tracing::warn!(
                                dest = %addr,
                                use_type = ?use_type,
                                error = ?err,
                                "socket operation denied by runtime policy"
                            );
                            return false;
                        }
                    }
                }
                SocketAddrUse::UdpBind | SocketAddrUse::UdpSend | SocketAddrUse::UdpReceive => {
                    if let Err(reason) = snapshot_check.check(addr, use_type) {
                        tracing::warn!(
                            dest = %addr,
                            use_type = ?use_type,
                            reason,
                            "socket operation denied by runtime policy"
                        );
                        return false;
                    }
                    false
                }
            };

            let extra_allowed = if let Some(check) = extra_check.as_ref() {
                // Cancellation while an asynchronous supervisor check is pending
                // must not leak the reservation made above.
                let mut reservation = PendingReservation {
                    enforcer: reserved_outbound_slot.then(|| policy_enforcer.clone()),
                };
                let allowed = check(addr, use_type).await;
                reservation.enforcer = None;
                allowed
            } else {
                true
            };

            if !extra_allowed {
                if use_type == SocketAddrUse::TcpConnect {
                    policy_enforcer
                        .counters
                        .connection_denied_total
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                if reserved_outbound_slot {
                    policy_enforcer.record_outbound_disconnect();
                }
                return false;
            }

            if reserved_outbound_slot {
                policy_enforcer.record_outbound_connect();
            }

            true
        })
    })
}

struct PendingReservation {
    enforcer: Option<PolicyEnforcer>,
}

impl Drop for PendingReservation {
    fn drop(&mut self) {
        if let Some(enforcer) = &self.enforcer {
            enforcer.record_outbound_disconnect();
        }
    }
}

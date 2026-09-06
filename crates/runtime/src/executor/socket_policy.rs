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

pub(crate) fn compose_socket_addr_check(
    policy_check: SocketPolicyCheck,
    policy_enforcer: PolicyEnforcer,
    extra_check: Option<SocketAddrCheckFn>,
) -> SocketAddrCheckFn {
    Arc::new(move |addr, use_type| {
        let policy_enforcer = policy_enforcer.clone();
        let snapshot_check = policy_check.clone();
        let extra_check = extra_check.clone();
        Box::pin(async move {
            let reserved_outbound_slot = match use_type {
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
                SocketAddrUse::TcpConnect => {
                    match policy_enforcer.check_outbound_tcp_connect(addr.ip(), addr.port()) {
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
                check(addr, use_type).await
            } else {
                true
            };

            if !extra_allowed {
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

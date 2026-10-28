use super::{validate_cidrs, InstancePolicy, NetworkPolicy};
use crate::types::AppConfig;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};

/// Opt-in restrictive network profiles; absent keeps the legacy policy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkProfile {
    Isolated,
    MeshOnly,
    EgressAllowlist,
}

pub fn canonical_http_host(host: &str) -> Result<String, String> {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty()
        || host.len() > 253
        || host.parse::<IpAddr>().is_ok()
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
    {
        return Err(format!("expected an exact DNS hostname, got {host:?}"));
    }
    Ok(host.to_ascii_lowercase())
}

pub fn is_mesh_http_host(host: &str) -> bool {
    canonical_http_host(host)
        .is_ok_and(|host| host.ends_with(".internal") && host.split('.').count() >= 3)
}

pub fn is_mesh_address(address: SocketAddr) -> bool {
    address.ip().to_canonical() == IpAddr::from([127, 0, 0, 1])
        && address.port() == crate::INTERNAL_GATEWAY_PORT
}

impl NetworkPolicy {
    /// Additional profile constraints; CIDR and operator checks still apply.
    pub fn check_profile_destination(
        &self,
        address: SocketAddr,
        http: bool,
    ) -> Result<(), &'static str> {
        let Some(profile) = self.profile else {
            return Ok(());
        };
        if profile == NetworkProfile::Isolated {
            return Err("isolated profile denies outbound traffic");
        }
        if is_mesh_address(address) {
            return Ok(());
        }
        if profile == NetworkProfile::MeshOnly {
            return Err("mesh-only permits only the local mesh gateway");
        }
        if !http
            && !self.allowed_tcp_endpoints.contains(&SocketAddr::new(
                address.ip().to_canonical(),
                address.port(),
            ))
        {
            return Err("egress-allowlist denies direct raw socket connections");
        }
        let ip = address.ip().to_canonical();
        let prohibited = match ip {
            IpAddr::V4(ip) => {
                ip.is_loopback()
                    || ip.is_link_local()
                    || ip.is_multicast()
                    || ip.is_broadcast()
                    || ip.octets()[0] == 0
                    || ip.octets()[0] >= 240
                    || ip.octets()[..2] == [100, 100] && ip.octets()[2..] == [100, 200]
            }
            IpAddr::V6(ip) => {
                ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_unicast_link_local()
                    || ip.is_multicast()
            }
        };
        if prohibited {
            return Err("profile prohibits reserved or metadata destination");
        }
        Ok(())
    }

    pub(super) fn validate_profile(&mut self) -> Result<(), String> {
        self.allowed_http_hosts = self
            .allowed_http_hosts
            .iter()
            .map(|host| canonical_http_host(host))
            .collect::<Result<_, _>>()?;
        if !self.allowed_tcp_endpoints.is_empty()
            && self.profile != Some(NetworkProfile::EgressAllowlist)
        {
            return Err("allowed_tcp_endpoints requires egress-allowlist".into());
        }
        let Some(profile) = self.profile else {
            return Ok(());
        };
        if self.allow_outbound_udp {
            return Err("restrictive profiles prohibit UDP".into());
        }
        if self.allow_dns {
            return Err("restrictive profiles prohibit raw WASI DNS; approved HTTP names resolve inside the HTTP transport".into());
        }
        match profile {
            NetworkProfile::Isolated if self.allow_outbound_tcp || self.allow_dns => {
                return Err("isolated profile prohibits outbound TCP and DNS".into())
            }
            NetworkProfile::MeshOnly if self.allow_dns => {
                return Err(
                    "mesh-only prohibits DNS; qualified .internal HTTP names resolve locally"
                        .into(),
                )
            }
            NetworkProfile::EgressAllowlist => {
                if self.allowed_cidrs.is_empty()
                    || self.allowed_http_hosts.is_empty() && self.allowed_tcp_endpoints.is_empty()
                {
                    return Err(
                        "egress-allowlist requires allowed_cidrs and HTTP hosts or exact TCP endpoints".into(),
                    );
                }
                if self.allowed_cidrs.iter().any(|cidr| {
                    cidr.parse::<ipnet::IpNet>()
                        .is_ok_and(|net| net.prefix_len() == 0)
                }) {
                    return Err("egress-allowlist prohibits unrestricted /0 CIDRs".into());
                }
            }
            _ => {}
        }
        if self.allow_outbound_tcp && self.max_egress_bytes == 0 {
            return Err("restrictive profiles require a positive max_egress_bytes budget".into());
        }
        Ok(())
    }
}

/// Operator policy, loaded before restoration and applied to every connect.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkAdmissionConfig {
    #[serde(default)]
    pub protected_namespaces: Vec<String>,
    /// Control-plane addresses/ranges and other node-wide forbidden egress.
    #[serde(default)]
    pub denied_cidrs: Vec<String>,
}

impl NetworkAdmissionConfig {
    pub fn validate(&self) -> Result<(), String> {
        validate_cidrs(&self.denied_cidrs)?;
        if self.protected_namespaces.iter().any(|name| {
            name.is_empty() || name.contains('/') || name.bytes().any(|b| b.is_ascii_whitespace())
        }) {
            return Err("protected_namespaces requires nonempty namespace names".into());
        }
        Ok(())
    }

    pub fn validate_app(&self, config: &AppConfig) -> Result<(), String> {
        let policy = config.resolve_instance_policy(config.wasm_bind_port)?;
        if config.namespace != config.id.namespace() {
            return Err("application namespace must match its qualified ID".into());
        }
        if self
            .protected_namespaces
            .iter()
            .any(|ns| ns == config.id.namespace())
            && policy.network.profile.is_none()
        {
            return Err(
                "protected namespace requires an explicit restrictive network profile".into(),
            );
        }
        Ok(())
    }

    pub fn denies(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.denied_cidrs.iter().any(|cidr| {
            cidr.parse::<ipnet::IpNet>()
                .is_ok_and(|net| net.contains(&ip))
        })
    }

    pub fn effective_policy(&self, config: &AppConfig) -> Result<InstancePolicy, String> {
        self.validate_app(config)?;
        let mut policy = config.resolve_instance_policy(config.wasm_bind_port)?;
        policy
            .network
            .denied_cidrs
            .extend(self.denied_cidrs.clone());
        policy.network.denied_cidrs.sort();
        policy.network.denied_cidrs.dedup();
        Ok(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{NetworkPolicyConfig, PolicyConfig};
    use crate::types::AppId;

    #[test]
    fn explicit_extended_network_limits_intersect_without_weakening_policy() {
        // Zero means unlimited bytes, but zero connections denies all connects.
        for (policy_bytes, extended_bytes, expected_bytes) in [
            (0, None, 0),
            (0, Some(0), 0),
            (0, Some(500), 500),
            (1000, None, 1000),
            (1000, Some(0), 1000),
            (1000, Some(500), 500),
            (1000, Some(2000), 1000),
        ] {
            for (extended_connections, expected_connections) in
                [(None, 5), (Some(0), 0), (Some(2), 2), (Some(10), 5)]
            {
                let mut app = AppConfig::default_for(AppId::new("limits", "v1"));
                app.policy = Some(PolicyConfig {
                    network: Some(NetworkPolicyConfig {
                        max_egress_bytes: Some(policy_bytes),
                        max_outbound_connections: Some(5),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
                app.extended_limits = Some(
                    serde_json::from_value(serde_json::json!({
                        "max_net_egress_bytes": extended_bytes,
                        "max_outbound_connections": extended_connections,
                    }))
                    .unwrap(),
                );
                let policy = app.resolve_instance_policy(19000).unwrap();
                assert_eq!(policy.network.max_egress_bytes, expected_bytes);
                assert_eq!(
                    policy.network.max_outbound_connections,
                    expected_connections
                );
                assert_eq!(policy.network.allowed_bind_ports, vec![19000]);
            }
        }
        let mut app = AppConfig::default_for(AppId::new("legacy", "v1"));
        let legacy = app.resolve_instance_policy(8080).unwrap();
        app.extended_limits = Some(serde_json::from_value(serde_json::json!({})).unwrap());
        let empty = app.resolve_instance_policy(8080).unwrap();
        assert_eq!(
            empty.network.max_egress_bytes,
            legacy.network.max_egress_bytes
        );
        assert_eq!(
            empty.network.max_outbound_connections,
            legacy.network.max_outbound_connections
        );
    }

    fn config(profile: NetworkProfile) -> PolicyConfig {
        PolicyConfig {
            network: Some(NetworkPolicyConfig {
                profile: Some(profile),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn restrictive_profile_defaults_and_overrides_fail_closed() {
        let isolated = config(NetworkProfile::Isolated)
            .resolve(8000)
            .unwrap()
            .network;
        assert!(
            !isolated.allow_outbound_tcp && !isolated.allow_dns && !isolated.allow_outbound_udp
        );
        let mesh = config(NetworkProfile::MeshOnly)
            .resolve(8000)
            .unwrap()
            .network;
        for address in ["127.0.0.1:9080", "[::ffff:127.0.0.1]:9080"] {
            assert!(mesh
                .check_profile_destination(address.parse().unwrap(), false)
                .is_ok());
        }
        for address in [
            "127.0.0.1:4222",
            "127.0.0.2:9080",
            "169.254.169.254:80",
            "1.1.1.1:443",
        ] {
            assert!(mesh
                .check_profile_destination(address.parse().unwrap(), true)
                .is_err());
        }
        let mut bad = config(NetworkProfile::MeshOnly);
        bad.network.as_mut().unwrap().allow_dns = Some(true);
        assert!(bad.resolve(8000).is_err());
        let mut bad = config(NetworkProfile::Isolated);
        bad.network.as_mut().unwrap().allow_outbound_tcp = Some(true);
        assert!(bad.resolve(8000).is_err());
    }

    #[test]
    fn egress_profile_requires_bounded_explicit_hosts_and_cidrs() {
        let mut cfg = config(NetworkProfile::EgressAllowlist);
        assert!(cfg.resolve(8000).is_err());
        let net = cfg.network.as_mut().unwrap();
        net.allowed_cidrs = Some(vec!["203.0.113.0/24".into()]);
        net.allowed_http_hosts = Some(vec!["Models.Example.COM.".into()]);
        let policy = cfg.resolve(8000).unwrap().network;
        assert_eq!(policy.allowed_http_hosts, vec!["models.example.com"]);
        assert!(policy
            .check_profile_destination("203.0.113.4:443".parse().unwrap(), true)
            .is_ok());
        assert!(policy
            .check_profile_destination("203.0.113.4:443".parse().unwrap(), false)
            .is_err());
        for address in [
            "127.0.0.1:4222",
            "169.254.169.254:80",
            "100.100.100.200:80",
            "[fe80::1]:443",
            "[::ffff:127.0.0.1]:443",
        ] {
            assert!(policy
                .check_profile_destination(address.parse().unwrap(), true)
                .is_err());
        }
        cfg.network.as_mut().unwrap().max_egress_bytes = Some(0);
        assert!(cfg.resolve(8000).is_err());
        cfg.network.as_mut().unwrap().max_egress_bytes = Some(1024);
        cfg.network.as_mut().unwrap().allowed_cidrs = Some(vec!["0.0.0.0/0".into()]);
        assert!(cfg.resolve(8000).is_err());
        for host in [
            "*.example.com",
            "http://example.com",
            "example.com:443",
            "127.0.0.1",
            "example..com",
            "bad-.example",
        ] {
            assert!(canonical_http_host(host).is_err(), "{host}");
        }
    }

    #[test]
    fn protected_admission_and_operator_denies_survive_effective_policy_resolution() {
        let admission = NetworkAdmissionConfig {
            protected_namespaces: vec!["agents".into()],
            denied_cidrs: vec!["172.20.0.10/32".into()],
        };
        let mut app = AppConfig::default_for(AppId::new_namespaced("agents", "harness", "v1"));
        assert!(admission.validate_app(&app).is_err());
        app.policy = Some(config(NetworkProfile::MeshOnly));
        admission.validate_app(&app).unwrap();
        assert!(admission
            .effective_policy(&app)
            .unwrap()
            .network
            .denied_cidrs
            .contains(&"172.20.0.10/32".into()));
        assert!(admission.denies("::ffff:172.20.0.10".parse().unwrap()));
        app.namespace = "default".into();
        assert!(admission.validate_app(&app).is_err());
    }

    #[test]
    fn caller_lists_distinguish_legacy_empty_and_exact_versioned_identities() {
        let mut route = crate::types::GatewayRouteConfig::default();
        assert!(route.caller_allowed(None));
        route.allowed_callers = Some(vec![]);
        assert!(!route.caller_allowed(None));
        assert!(!route.caller_allowed(Some("agents/researcher:v1")));
        route.allowed_callers = Some(vec!["agents/researcher:v1".into()]);
        assert!(route.caller_allowed(Some("agents/researcher:v1")));
        for caller in [
            None,
            Some("agents/researcher:v2"),
            Some("other/researcher:v1"),
        ] {
            assert!(!route.caller_allowed(caller));
        }
        for caller in [
            "researcher:v1",
            "agents/:v1",
            "agents:researcher/v1",
            "agents/*:v1",
            "agents/a:v1:v2",
        ] {
            route.allowed_callers = Some(vec![caller.into()]);
            assert!(route.validate_allowed_callers().is_err(), "{caller}");
            assert!(!route.caller_allowed(Some(caller)));
        }
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;
    use crate::policy::{NetworkPolicyConfig, PolicyConfig};

    #[test]
    fn database_tcp_exception_is_exact_and_does_not_grant_http_or_dns() {
        let policy = PolicyConfig {
            network: Some(NetworkPolicyConfig {
                profile: Some(NetworkProfile::EgressAllowlist),
                allowed_cidrs: Some(vec!["172.20.0.20/32".into()]),
                allowed_tcp_endpoints: Some(vec!["172.20.0.20:5432".into()]),
                ..Default::default()
            }),
            ..Default::default()
        }
        .resolve(8080)
        .unwrap()
        .network;
        assert!(!policy.allow_dns);
        assert!(policy.allowed_http_hosts.is_empty());
        assert!(policy
            .check_profile_destination("172.20.0.20:5432".parse().unwrap(), false)
            .is_ok());
        for address in ["172.20.0.20:4222", "172.20.0.21:5432"] {
            assert!(policy
                .check_profile_destination(address.parse().unwrap(), false)
                .is_err());
        }
    }
}

//! WASI policy enforcement data structures.
//!
//! These types define the resource and network policies for Wasm applications,
//! which are enforced at the WASI host layer.

use serde::{Deserialize, Serialize};

/// Network policy for a single Wasm app instance.
/// Enforced at the WASI host layer before any network operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkPolicy {
    /// Allow outbound TCP connections.
    pub allow_outbound_tcp: bool,

    /// Allow outbound UDP.
    pub allow_outbound_udp: bool,

    /// Allow DNS resolution (IP name lookup).
    pub allow_dns: bool,

    /// Allowed destination CIDRs for outbound connections.
    /// Empty = all destinations allowed (if allow_outbound_tcp/udp is true).
    /// Non-empty = only these CIDRs are allowed.
    pub allowed_cidrs: Vec<String>,

    /// Denied destination CIDRs (takes precedence over allowed_cidrs).
    /// Useful for blocking specific internal ranges (e.g., metadata service).
    pub denied_cidrs: Vec<String>,

    /// Maximum concurrent outbound connections.
    pub max_outbound_connections: u32,

    /// Maximum total egress bytes (0 = unlimited).
    pub max_egress_bytes: u64,

    /// Ports the app is allowed to bind to.
    /// Normally just one: the pre-bound port from the Supervisor.
    pub allowed_bind_ports: Vec<u16>,

    /// Allow inbound connections (for the app's HTTP server).
    /// This should always be true for apps that receive requests.
    pub allow_inbound: bool,
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        NetworkPolicy {
            allow_outbound_tcp: true,
            allow_outbound_udp: false,
            allow_dns: true,
            allowed_cidrs: Vec::new(),
            denied_cidrs: Vec::new(),
            max_outbound_connections: 100,
            max_egress_bytes: 0,            // unlimited by default
            allowed_bind_ports: Vec::new(), // populated at spawn time
            allow_inbound: true,
        }
    }
}

/// Filesystem and I/O policy for a single Wasm app instance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilesystemPolicy {
    /// Maximum number of simultaneously open file descriptors.
    pub max_open_fds: u32,

    /// Maximum total bytes written to the filesystem (0 = unlimited).
    pub max_fs_write_bytes: u64,

    /// Maximum total bytes read from the filesystem (0 = unlimited).
    pub max_fs_read_bytes: u64,

    /// Allow the app to create new files.
    pub allow_file_create: bool,

    /// Allow the app to delete files.
    pub allow_file_delete: bool,

    /// Allowed directories (preopen paths). Empty = no filesystem access.
    pub allowed_paths: Vec<String>,
}

impl Default for FilesystemPolicy {
    fn default() -> Self {
        FilesystemPolicy {
            max_open_fds: 64,
            max_fs_write_bytes: 50 * 1024 * 1024, // 50 MB
            max_fs_read_bytes: 0,                 // unlimited
            allow_file_create: false,
            allow_file_delete: false,
            allowed_paths: Vec::new(), // no filesystem by default
        }
    }
}

/// Combined policy for a Wasm instance.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstancePolicy {
    pub network: NetworkPolicy,
    pub filesystem: FilesystemPolicy,
}

/// Policy configuration stored in AppConfig (operator-facing).
/// Resolved into InstancePolicy at spawn time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyConfig {
    /// Network policy overrides. None = use defaults.
    #[serde(default)]
    pub network: Option<NetworkPolicyConfig>,

    /// Filesystem policy overrides. None = use defaults.
    #[serde(default)]
    pub filesystem: Option<FilesystemPolicyConfig>,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        PolicyConfig {
            network: None,
            filesystem: None,
        }
    }
}

/// Operator-facing network policy config (in TOML / deploy manifest).
/// All fields are optional — None means "use the platform default".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetworkPolicyConfig {
    pub allow_outbound_tcp: Option<bool>,
    pub allow_outbound_udp: Option<bool>,
    pub allow_dns: Option<bool>,
    pub allowed_cidrs: Option<Vec<String>>,
    pub denied_cidrs: Option<Vec<String>>,
    pub max_outbound_connections: Option<u32>,
    pub max_egress_bytes: Option<u64>,
}

/// Operator-facing filesystem policy config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilesystemPolicyConfig {
    pub max_open_fds: Option<u32>,
    pub max_fs_write_bytes: Option<u64>,
    pub max_fs_read_bytes: Option<u64>,
    pub allow_file_create: Option<bool>,
    pub allow_file_delete: Option<bool>,
    pub allowed_paths: Option<Vec<String>>,
}

impl PolicyConfig {
    /// Resolve this config into a full InstancePolicy, applying defaults
    /// for any fields not explicitly set.
    pub fn resolve(&self, assigned_port: u16) -> InstancePolicy {
        let net_default = NetworkPolicy::default();
        let fs_default = FilesystemPolicy::default();

        let network = match &self.network {
            Some(cfg) => NetworkPolicy {
                allow_outbound_tcp: cfg
                    .allow_outbound_tcp
                    .unwrap_or(net_default.allow_outbound_tcp),
                allow_outbound_udp: cfg
                    .allow_outbound_udp
                    .unwrap_or(net_default.allow_outbound_udp),
                allow_dns: cfg.allow_dns.unwrap_or(net_default.allow_dns),
                allowed_cidrs: cfg
                    .allowed_cidrs
                    .clone()
                    .unwrap_or(net_default.allowed_cidrs),
                denied_cidrs: cfg.denied_cidrs.clone().unwrap_or(net_default.denied_cidrs),
                max_outbound_connections: cfg
                    .max_outbound_connections
                    .unwrap_or(net_default.max_outbound_connections),
                max_egress_bytes: cfg.max_egress_bytes.unwrap_or(net_default.max_egress_bytes),
                allowed_bind_ports: vec![assigned_port],
                allow_inbound: true,
            },
            None => NetworkPolicy {
                allowed_bind_ports: vec![assigned_port],
                ..net_default
            },
        };

        let filesystem = match &self.filesystem {
            Some(cfg) => FilesystemPolicy {
                max_open_fds: cfg.max_open_fds.unwrap_or(fs_default.max_open_fds),
                max_fs_write_bytes: cfg
                    .max_fs_write_bytes
                    .unwrap_or(fs_default.max_fs_write_bytes),
                max_fs_read_bytes: cfg
                    .max_fs_read_bytes
                    .unwrap_or(fs_default.max_fs_read_bytes),
                allow_file_create: cfg
                    .allow_file_create
                    .unwrap_or(fs_default.allow_file_create),
                allow_file_delete: cfg
                    .allow_file_delete
                    .unwrap_or(fs_default.allow_file_delete),
                allowed_paths: cfg
                    .allowed_paths
                    .clone()
                    .unwrap_or(fs_default.allowed_paths),
            },
            None => fs_default,
        };

        InstancePolicy {
            network,
            filesystem,
        }
    }
}

/// Pre-defined policy profiles for common application types.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PolicyProfile {
    /// HTTP API: inbound allowed, outbound TCP/DNS allowed, no filesystem.
    HttpApi,
    /// Background worker: no inbound, outbound TCP/DNS allowed, no filesystem.
    BackgroundWorker,
    /// Static site: inbound allowed, no outbound, read-only filesystem.
    StaticSite,
    /// Database proxy: inbound allowed, outbound TCP to database CIDRs only.
    DatabaseProxy,
    /// Unrestricted: everything allowed (for trusted internal apps).
    Unrestricted,
}

impl PolicyProfile {
    /// Convert a profile to a concrete PolicyConfig.
    pub fn to_config(&self) -> PolicyConfig {
        match self {
            PolicyProfile::HttpApi => PolicyConfig {
                network: Some(NetworkPolicyConfig {
                    allow_outbound_tcp: Some(true),
                    allow_outbound_udp: Some(false),
                    allow_dns: Some(true),
                    allowed_cidrs: None,
                    denied_cidrs: None,
                    max_outbound_connections: Some(100),
                    max_egress_bytes: Some(0),
                }),
                filesystem: Some(FilesystemPolicyConfig {
                    max_open_fds: Some(64),
                    max_fs_write_bytes: Some(0),
                    max_fs_read_bytes: Some(0),
                    allow_file_create: Some(false),
                    allow_file_delete: Some(false),
                    allowed_paths: Some(Vec::new()),
                }),
            },
            PolicyProfile::BackgroundWorker => PolicyConfig {
                network: Some(NetworkPolicyConfig {
                    allow_outbound_tcp: Some(true),
                    allow_outbound_udp: Some(false),
                    allow_dns: Some(true),
                    allowed_cidrs: None,
                    denied_cidrs: None,
                    max_outbound_connections: Some(50),
                    max_egress_bytes: Some(0),
                }),
                filesystem: Some(FilesystemPolicyConfig {
                    max_open_fds: Some(32),
                    max_fs_write_bytes: Some(10 * 1024 * 1024), // 10 MB
                    max_fs_read_bytes: Some(0),
                    allow_file_create: Some(false),
                    allow_file_delete: Some(false),
                    allowed_paths: Some(Vec::new()),
                }),
            },
            PolicyProfile::StaticSite => PolicyConfig {
                network: Some(NetworkPolicyConfig {
                    allow_outbound_tcp: Some(false),
                    allow_outbound_udp: Some(false),
                    allow_dns: Some(false),
                    allowed_cidrs: None,
                    denied_cidrs: None,
                    max_outbound_connections: Some(0),
                    max_egress_bytes: Some(0),
                }),
                filesystem: Some(FilesystemPolicyConfig {
                    max_open_fds: Some(32),
                    max_fs_write_bytes: Some(0),
                    max_fs_read_bytes: Some(100 * 1024 * 1024), // 100 MB read
                    allow_file_create: Some(false),
                    allow_file_delete: Some(false),
                    allowed_paths: Some(vec!["/static".to_string()]),
                }),
            },
            PolicyProfile::DatabaseProxy => PolicyConfig {
                network: Some(NetworkPolicyConfig {
                    allow_outbound_tcp: Some(true),
                    allow_outbound_udp: Some(false),
                    allow_dns: Some(true),
                    allowed_cidrs: Some(vec![
                        "10.0.0.0/8".to_string(),
                        "172.16.0.0/12".to_string(),
                        "192.168.0.0/16".to_string(),
                    ]),
                    denied_cidrs: None,
                    max_outbound_connections: Some(200),
                    max_egress_bytes: Some(0),
                }),
                filesystem: Some(FilesystemPolicyConfig {
                    max_open_fds: Some(128),
                    max_fs_write_bytes: Some(0),
                    max_fs_read_bytes: Some(0),
                    allow_file_create: Some(false),
                    allow_file_delete: Some(false),
                    allowed_paths: Some(Vec::new()),
                }),
            },
            PolicyProfile::Unrestricted => PolicyConfig {
                network: Some(NetworkPolicyConfig {
                    allow_outbound_tcp: Some(true),
                    allow_outbound_udp: Some(true),
                    allow_dns: Some(true),
                    allowed_cidrs: None,
                    denied_cidrs: None,
                    max_outbound_connections: None,
                    max_egress_bytes: Some(0),
                }),
                filesystem: Some(FilesystemPolicyConfig {
                    max_open_fds: None,
                    max_fs_write_bytes: Some(0),
                    max_fs_read_bytes: Some(0),
                    allow_file_create: Some(true),
                    allow_file_delete: Some(true),
                    allowed_paths: Some(vec!["/".to_string()]),
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_network_policy_default() {
        let policy = NetworkPolicy::default();
        assert!(policy.allow_outbound_tcp);
        assert!(!policy.allow_outbound_udp);
        assert!(policy.allow_dns);
        assert!(policy.allowed_cidrs.is_empty());
        assert!(policy.denied_cidrs.is_empty());
        assert_eq!(policy.max_outbound_connections, 100);
        assert_eq!(policy.max_egress_bytes, 0);
        assert!(policy.allowed_bind_ports.is_empty());
        assert!(policy.allow_inbound);
    }

    #[test]
    fn test_filesystem_policy_default() {
        let policy = FilesystemPolicy::default();
        assert_eq!(policy.max_open_fds, 64);
        assert_eq!(policy.max_fs_write_bytes, 50 * 1024 * 1024);
        assert_eq!(policy.max_fs_read_bytes, 0);
        assert!(!policy.allow_file_create);
        assert!(!policy.allow_file_delete);
        assert!(policy.allowed_paths.is_empty());
    }

    #[test]
    fn test_policy_config_resolve_defaults() {
        let config = PolicyConfig::default();
        let instance_policy = config.resolve(8080);
        assert_eq!(instance_policy.network.allowed_bind_ports, vec![8080]);
        assert!(instance_policy.network.allow_outbound_tcp);
        assert!(!instance_policy.network.allow_outbound_udp);
        assert!(instance_policy.network.allow_dns);
        assert_eq!(instance_policy.filesystem.max_open_fds, 64);
    }

    #[test]
    fn test_policy_config_resolve_with_overrides() {
        let config = PolicyConfig {
            network: Some(NetworkPolicyConfig {
                allow_outbound_tcp: Some(false),
                allow_outbound_udp: Some(true),
                allow_dns: Some(false),
                allowed_cidrs: Some(vec!["10.0.0.0/8".to_string()]),
                denied_cidrs: Some(vec!["10.1.0.0/16".to_string()]),
                max_outbound_connections: Some(50),
                max_egress_bytes: Some(1024 * 1024),
            }),
            filesystem: Some(FilesystemPolicyConfig {
                max_open_fds: Some(128),
                max_fs_write_bytes: Some(100 * 1024 * 1024),
                max_fs_read_bytes: Some(200 * 1024 * 1024),
                allow_file_create: Some(true),
                allow_file_delete: Some(false),
                allowed_paths: Some(vec!["/tmp".to_string()]),
            }),
        };
        let instance_policy = config.resolve(9090);
        assert_eq!(instance_policy.network.allowed_bind_ports, vec![9090]);
        assert!(!instance_policy.network.allow_outbound_tcp);
        assert!(instance_policy.network.allow_outbound_udp);
        assert!(!instance_policy.network.allow_dns);
        assert_eq!(
            instance_policy.network.allowed_cidrs,
            vec!["10.0.0.0/8".to_string()]
        );
        assert_eq!(
            instance_policy.network.denied_cidrs,
            vec!["10.1.0.0/16".to_string()]
        );
        assert_eq!(instance_policy.network.max_outbound_connections, 50);
        assert_eq!(instance_policy.network.max_egress_bytes, 1024 * 1024);
        assert_eq!(instance_policy.filesystem.max_open_fds, 128);
        assert_eq!(
            instance_policy.filesystem.max_fs_write_bytes,
            100 * 1024 * 1024
        );
        assert_eq!(
            instance_policy.filesystem.max_fs_read_bytes,
            200 * 1024 * 1024
        );
        assert!(instance_policy.filesystem.allow_file_create);
        assert!(!instance_policy.filesystem.allow_file_delete);
        assert_eq!(
            instance_policy.filesystem.allowed_paths,
            vec!["/tmp".to_string()]
        );
    }

    #[test]
    fn test_policy_profile_http_api() {
        let config = PolicyProfile::HttpApi.to_config();
        assert!(config.network.is_some());
        let network = config.network.unwrap();
        assert_eq!(network.allow_outbound_tcp, Some(true));
        assert_eq!(network.allow_outbound_udp, Some(false));
        assert_eq!(network.allow_dns, Some(true));
        assert_eq!(network.max_outbound_connections, Some(100));
        assert_eq!(network.max_egress_bytes, Some(0));
        let filesystem = config.filesystem.unwrap();
        assert_eq!(filesystem.max_open_fds, Some(64));
        assert_eq!(filesystem.max_fs_write_bytes, Some(0));
        assert_eq!(filesystem.max_fs_read_bytes, Some(0));
        assert_eq!(filesystem.allow_file_create, Some(false));
        assert_eq!(filesystem.allow_file_delete, Some(false));
        assert_eq!(filesystem.allowed_paths, Some(Vec::new()));
    }

    #[test]
    fn test_policy_profile_background_worker() {
        let config = PolicyProfile::BackgroundWorker.to_config();
        assert!(config.network.is_some());
        let network = config.network.unwrap();
        assert_eq!(network.allow_outbound_tcp, Some(true));
        assert_eq!(network.allow_outbound_udp, Some(false));
        assert_eq!(network.allow_dns, Some(true));
        assert_eq!(network.max_outbound_connections, Some(50));
        let filesystem = config.filesystem.unwrap();
        assert_eq!(filesystem.max_open_fds, Some(32));
        assert_eq!(filesystem.max_fs_write_bytes, Some(10 * 1024 * 1024));
    }

    #[test]
    fn test_policy_profile_static_site() {
        let config = PolicyProfile::StaticSite.to_config();
        assert!(config.network.is_some());
        let network = config.network.unwrap();
        assert_eq!(network.allow_outbound_tcp, Some(false));
        assert_eq!(network.allow_outbound_udp, Some(false));
        assert_eq!(network.allow_dns, Some(false));
        assert_eq!(network.max_outbound_connections, Some(0));
        let filesystem = config.filesystem.unwrap();
        assert_eq!(filesystem.max_fs_read_bytes, Some(100 * 1024 * 1024));
        assert_eq!(filesystem.allowed_paths, Some(vec!["/static".to_string()]));
    }

    #[test]
    fn test_policy_profile_database_proxy() {
        let config = PolicyProfile::DatabaseProxy.to_config();
        assert!(config.network.is_some());
        let network = config.network.unwrap();
        assert_eq!(network.allow_outbound_tcp, Some(true));
        assert_eq!(network.allow_outbound_udp, Some(false));
        assert_eq!(network.allow_dns, Some(true));
        assert_eq!(
            network.allowed_cidrs,
            Some(vec![
                "10.0.0.0/8".to_string(),
                "172.16.0.0/12".to_string(),
                "192.168.0.0/16".to_string(),
            ])
        );
        assert_eq!(network.max_outbound_connections, Some(200));
    }

    #[test]
    fn test_policy_profile_unrestricted() {
        let config = PolicyProfile::Unrestricted.to_config();
        assert!(config.network.is_some());
        let network = config.network.unwrap();
        assert_eq!(network.allow_outbound_tcp, Some(true));
        assert_eq!(network.allow_outbound_udp, Some(true));
        assert_eq!(network.allow_dns, Some(true));
        assert_eq!(network.max_egress_bytes, Some(0));
        let filesystem = config.filesystem.unwrap();
        assert_eq!(filesystem.max_fs_write_bytes, Some(0));
        assert_eq!(filesystem.max_fs_read_bytes, Some(0));
        assert_eq!(filesystem.allow_file_create, Some(true));
        assert_eq!(filesystem.allow_file_delete, Some(true));
        assert_eq!(filesystem.allowed_paths, Some(vec!["/".to_string()]));
    }
}

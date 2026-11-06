//! Least-privilege service credentials for the generic application control API.
//! Independent of agent tools and of the legacy global administrator tokens.
use crate::{crypto::constant_time_eq, types::AppId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Observe,
    Restart,
    Configure,
    Deploy,
    NodeObserve,
    NodeManage,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Grant {
    pub principal: String,
    pub token: String,
    pub applications: BTreeSet<String>,
    pub permissions: BTreeSet<Permission>,
    #[serde(default)]
    pub nodes: BTreeSet<String>,
    #[serde(default)]
    pub secret_keys: BTreeSet<String>,
    #[serde(default)]
    pub route_hosts: BTreeSet<String>,
}
impl Grant {
    pub fn allows(&self, app: &str, permission: Permission) -> bool {
        self.applications.contains(app) && self.permissions.contains(&permission)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grants {
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub pricing: Option<Pricing>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Pricing {
    pub currency: String,
    pub fuel_micros_per_million: u64,
    pub memory_micros_per_gib_second: u64,
}
pub fn valid_application(app: &str) -> bool {
    let segment = |s: &str| {
        !s.is_empty()
            && s.len() <= 128
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    app.len() <= 256
        && app.split_once('/').is_some_and(|(ns, name)| {
            segment(ns)
                && name
                    .split_once(':')
                    .is_some_and(|(name, version)| segment(name) && segment(version))
        })
        && AppId::new_validate(app).is_ok()
}
impl Grants {
    pub fn valid(&self) -> bool {
        let mut tokens = BTreeSet::new();
        let mut principals = BTreeSet::new();
        !self.grants.is_empty()
            && self.pricing.as_ref().is_none_or(|p| {
                p.currency.len() == 3
                    && p.currency.bytes().all(|b| b.is_ascii_uppercase())
                    && p.fuel_micros_per_million <= 1_000_000_000
                    && p.memory_micros_per_gib_second <= 1_000_000_000
            })
            && self.grants.len() <= 64
            && self.grants.iter().all(|g| {
                !g.principal.is_empty()
                    && g.principal.len() <= 128
                    && g.principal
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                    && principals.insert(&g.principal)
                    && g.token.len() == 64
                    && g.token.bytes().all(|b| b.is_ascii_hexdigit())
                    && tokens.insert(&g.token)
                    && !g.permissions.is_empty()
                    && (!g.applications.is_empty() || !g.nodes.is_empty())
                    && g.applications.len() <= 32
                    && g.applications.iter().all(|app| valid_application(app))
                    && g.nodes.len() <= 32
                    && g.secret_keys.len() <= 128
                    && g.secret_keys.iter().all(|key| {
                        !key.is_empty()
                            && key.len() <= 128
                            && key
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b))
                    })
                    && g.nodes.iter().all(|n| {
                        !n.is_empty()
                            && n.len() <= 128
                            && n.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                    })
                    && g.route_hosts.len() <= 32
                    && g.route_hosts.iter().all(|h| {
                        !h.is_empty()
                            && h.len() <= 253
                            && !h.ends_with(".internal")
                            && h.bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
                    })
            })
    }
    pub fn authenticate(&self, token: &str) -> Option<Grant> {
        if token.len() != 64 {
            return None;
        }
        // Compare every configured credential; never log or serialize them.
        self.grants.iter().fold(None, |found, grant| {
            if constant_time_eq(token.as_bytes(), grant.token.as_bytes()) {
                Some(grant.clone())
            } else {
                found
            }
        })
    }
}

/// Closed, credential-free changes to an existing exact application identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Change {
    Configure {
        key: String,
        value: u64,
    },
    Scale {
        replicas: u32,
    },
    Deploy {
        artifact_sha256: String,
        #[serde(default)]
        config: Option<Box<crate::types::AppConfig>>,
    },
    Rollback {
        revision: String,
    },
    Route {
        host: String,
        path_prefix: String,
        strip_prefix: bool,
    },
    Gateway {
        config: Box<crate::types::GatewayRouteConfig>,
    },
    GatewayReset,
    Trigger {
        id: String,
        paused: bool,
    },
}
impl Change {
    pub fn permission(&self) -> Permission {
        match self {
            Self::Deploy { .. } | Self::Rollback { .. } => Permission::Deploy,
            _ => Permission::Configure,
        }
    }
    pub fn valid(&self) -> bool {
        let hash = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
        match self {
            Self::Configure { key, value } => match key.as_str() {
                "fuel_quota" => (1..=10_000_000_000).contains(value),
                "memory_pages" => (1..=8192).contains(value),
                "max_instances" => (1..=100).contains(value),
                "idle_timeout_secs" => (1..=86400).contains(value),
                _ => false,
            },
            Self::Scale { replicas } => *replicas <= 100,
            Self::Deploy {
                artifact_sha256, ..
            } => hash(artifact_sha256),
            Self::Rollback { revision } => {
                hash(revision)
                    || (!revision.is_empty()
                        && revision.len() <= 128
                        && revision
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)))
            }
            Self::Route {
                host, path_prefix, ..
            } => {
                !host.is_empty()
                    && host.len() <= 253
                    && host
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
                    && !host.contains("..")
                    && path_prefix.starts_with('/')
                    && path_prefix.len() <= 256
                    && !path_prefix
                        .bytes()
                        .any(|b| b.is_ascii_control() || b"?#\\".contains(&b))
            }
            Self::Gateway { config } => config.validate_allowed_callers().is_ok(),
            Self::GatewayReset => true,
            Self::Trigger { id, .. } => {
                !id.is_empty()
                    && id.len() <= 64
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            }
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanRequest {
    pub expected: Fence,
    pub change: Change,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Apply {
    pub operation_id: String,
    pub plan_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Fence {
    pub revision: String,
    pub instances: BTreeSet<String>,
}
impl Fence {
    pub fn valid(&self) -> bool {
        self.revision.len() == 64
            && self.revision.bytes().all(|b| b.is_ascii_hexdigit())
            && self.instances.len() <= 128
            && self
                .instances
                .iter()
                .all(|id| uuid::Uuid::parse_str(id).is_ok())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Restart {
    pub operation_id: String,
    pub expected: Fence,
}
pub fn valid_operation_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_:".contains(&b))
}
impl Restart {
    pub fn valid(&self) -> bool {
        valid_operation_key(&self.operation_id)
            && self.expected.valid()
            && !self.expected.instances.is_empty()
            && self.expected.instances.len() <= 128
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn grants() -> Grants {
        serde_json::from_value(json!({"grants":[{"principal":"observer","token":"a".repeat(64),"applications":["test/example:v1"],"permissions":["observe"]}]})).unwrap()
    }
    #[test]
    fn service_credentials_never_imply_global_or_write_authority() {
        let policy = grants();
        assert!(policy.valid());
        let g = policy.authenticate(&"a".repeat(64)).unwrap();
        assert!(g.allows("test/example:v1", Permission::Observe));
        assert!(!g.allows("test/other:v1", Permission::Observe));
        assert!(!g.allows("test/example:v1", Permission::Restart));
        assert!(policy.authenticate(&"b".repeat(64)).is_none());
        let mut duplicate = grants();
        duplicate.grants.push(duplicate.grants[0].clone());
        assert!(!duplicate.valid());
    }
    #[test]
    fn invalid_scopes_and_unbounded_restart_preconditions_are_rejected() {
        let mut policy = grants();
        policy.grants[0].applications = ["test/*".into()].into();
        assert!(!policy.valid());
        policy.grants[0].applications = ["example:v1".into()].into();
        assert!(!policy.valid());
        let request = Restart {
            operation_id: "op".into(),
            expected: Fence {
                revision: "b".repeat(64),
                instances: BTreeSet::new(),
            },
        };
        assert!(!request.valid());
    }
}

use super::*;
use std::collections::HashMap;

#[test]
fn test_manifest_parse_minimal() {
    let toml = r#"
[app]
name = "test-app"
version = "v1"
namespace = "production"
wasm_artifact = "./test.wasm"

[fuel]
quota = 100000000
memory_pages = 512
max_instances = 5
idle_timeout_secs = 60
"#;
    let manifest: DeployManifest = toml::from_str(toml).unwrap();
    assert_eq!(manifest.app.name, "test-app");
    assert_eq!(manifest.app.version, "v1");
    assert_eq!(manifest.app.namespace, "production");
    assert_eq!(manifest.app.wasm_artifact, "./test.wasm");
    assert_eq!(manifest.fuel.quota, 100000000);
    assert_eq!(manifest.fuel.memory_pages, 512);
    assert!(manifest.artifact.is_none());
}

#[test]
fn test_manifest_parse_with_gateway() {
    let toml = r#"
[app]
name = "api-users"
version = "v2"
wasm_artifact = "./api.wasm"

[gateway]
host = "api.example.com"

[gateway.auth]
policy = "roles"
allowed_roles = ["admin", "user"]

[[gateway.endpoints]]
path = "/health"
methods = ["GET"]
auth = "none"

[[gateway.endpoints]]
path = "/api/admin"
methods = ["POST"]
auth = "roles"
allowed_roles = ["admin"]
required_scopes = ["admin:users"]
"#;
    let manifest: DeployManifest = toml::from_str(toml).unwrap();
    let gw = manifest.gateway.as_ref().unwrap();
    assert_eq!(gw.host, Some("api.example.com".to_string()));
    assert!(gw.routes.is_empty());
    let auth = gw.auth.as_ref().unwrap();
    assert_eq!(auth.policy, "roles");
    assert_eq!(auth.allowed_roles, vec!["admin", "user"]);
    assert_eq!(gw.endpoints.len(), 2);
    assert_eq!(gw.endpoints[0].path, "/health");
    assert_eq!(gw.endpoints[0].auth, "none");
    assert_eq!(gw.endpoints[1].path, "/api/admin");
    assert_eq!(gw.endpoints[1].required_scopes, vec!["admin:users"]);
}

#[test]
fn test_manifest_to_app_config() {
    let manifest = DeployManifest {
        app: AppManifestSection {
            name: "test".to_string(),
            version: "v1".to_string(),
            namespace: "staging".to_string(),
            description: String::new(),
            wasm_artifact: "./test.wasm".to_string(),
            wasm_bind_port: 8080,
        },
        fuel: FuelManifestSection {
            quota: 1000000,
            memory_pages: 1024,
            max_instances: 3,
            idle_timeout_secs: 120,
        },
        policy: None,
        gateway: None,
        env: {
            let mut m = HashMap::new();
            m.insert("LOG_LEVEL".to_string(), "debug".to_string());
            m
        },
        secrets: HashMap::new(),
        api_keys: vec![],
        artifact: None,
    };

    let config = manifest.to_app_config();
    assert_eq!(config.id.0, "staging/test:v1");
    assert_eq!(config.namespace, "staging");
    assert_eq!(config.fuel_quota.0, 1000000);
    assert_eq!(config.memory_limit.0, 1024);
    assert_eq!(config.max_instances, 3);
    assert_eq!(config.env_vars.get("LOG_LEVEL"), Some(&"debug".to_string()));
}

#[test]
fn test_manifest_to_gateway_config() {
    let manifest = DeployManifest {
        app: AppManifestSection::default(),
        fuel: FuelManifestSection::default(),
        policy: None,
        gateway: Some(GatewayManifestSection {
            host: Some("api.example.com".to_string()),
            routes: Vec::new(),
            auth: Some(GatewayAuthManifest {
                policy: "authenticated".to_string(),
                allowed_roles: vec![],
                client_id: None,
            }),
            cors: None,
            rate_limit: Some(common::types::RouteRateLimit {
                requests_per_second: 100,
                burst_capacity: 20,
                distributed: false,
            }),
            circuit_breaker: None,
            transform: None,
            endpoints: vec![EndpointRuleManifest {
                path: "/health".to_string(),
                methods: vec!["GET".to_string()],
                auth: "none".to_string(),
                allowed_roles: vec![],
                required_scopes: vec!["status:read".to_string()],
                client_id: None,
                rate_limit: None,
            }],
        }),
        env: HashMap::new(),
        secrets: HashMap::new(),
        api_keys: vec![],
        artifact: None,
    };

    let gw = manifest.to_gateway_config().unwrap();
    assert_eq!(gw.auth, common::types::AuthPolicy::Authenticated);
    assert_eq!(gw.rate_limit.as_ref().unwrap().requests_per_second, 100);
    assert!(!gw.rate_limit.as_ref().unwrap().distributed);
    assert_eq!(gw.endpoints.len(), 1);
    assert_eq!(gw.endpoints[0].required_scopes, vec!["status:read"]);
}

#[test]
fn test_manifest_to_routes_supports_host_and_explicit_routes() {
    let toml = r#"
[app]
name = "api-users"
version = "v2"
namespace = "tenant-a"

[gateway]
host = "API.EXAMPLE.COM"

[[gateway.routes]]
host = "api.example.com"
path_prefix = "/v1"
strip_prefix = true
"#;

    let manifest: DeployManifest = toml::from_str(toml).unwrap();
    let app_id = common::types::AppId::new_namespaced("tenant-a", "api-users", "v2");
    let routes = manifest.to_routes(&app_id).unwrap();

    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].host, "api.example.com");
    assert_eq!(routes[0].path_prefix, "/");
    assert!(!routes[0].strip_prefix);
    assert_eq!(routes[1].host, "api.example.com");
    assert_eq!(routes[1].path_prefix, "/v1");
    assert!(routes[1].strip_prefix);
}

#[test]
fn test_manifest_to_routes_rejects_duplicate_host_and_prefix() {
    let toml = r#"
[app]
name = "api-users"
version = "v2"

[gateway]
host = "api.example.com"

[[gateway.routes]]
host = "api.example.com"
path_prefix = "/"
"#;

    let manifest: DeployManifest = toml::from_str(toml).unwrap();
    let app_id = common::types::AppId::new_namespaced("default", "api-users", "v2");
    let err = manifest.to_routes(&app_id).unwrap_err().to_string();
    assert!(err.contains("duplicate gateway route"));
}

#[test]
fn test_manifest_to_routes_normalizes_path_prefix() {
    let toml = r#"
[app]
name = "api-users"
version = "v2"

[[gateway.routes]]
host = "api.example.com"
path_prefix = "v1"
"#;

    let manifest: DeployManifest = toml::from_str(toml).unwrap();
    let app_id = common::types::AppId::new_namespaced("default", "api-users", "v2");
    let routes = manifest.to_routes(&app_id).unwrap();
    assert_eq!(routes[0].path_prefix, "/v1");
}

#[test]
fn test_manifest_rate_limit_defaults_to_node_local_when_distributed_is_omitted() {
    let toml = r#"
[app]
name = "api-users"
version = "v1"
wasm_artifact = "./api.wasm"

[gateway.rate_limit]
requests_per_second = 100
burst_capacity = 20
"#;

    let manifest: DeployManifest = toml::from_str(toml).unwrap();
    let rate_limit = manifest
        .gateway
        .as_ref()
        .and_then(|gateway| gateway.rate_limit.as_ref())
        .unwrap();
    assert!(!rate_limit.distributed);
}

#[test]
fn test_manifest_parse_remote_artifact_section() {
    let toml = r#"
[app]
name = "api-users"
version = "v1"

[artifact]
url = "https://example.com/api-users.wasm"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
credential_ref = "github-packages-reader"
"#;

    let manifest: DeployManifest = toml::from_str(toml).unwrap();
    let artifact = manifest.artifact.unwrap();
    assert_eq!(artifact.url, "https://example.com/api-users.wasm");
    assert_eq!(
        artifact.sha256,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    );
    assert_eq!(
        artifact.credential_ref.as_deref(),
        Some("github-packages-reader")
    );
    assert!(artifact.reference.is_none());
}

#[test]
fn test_manifest_parse_oci_artifact_reference_section() {
    let toml = r#"
[app]
name = "api-users"
version = "v1"

[artifact]
reference = "oci://ghcr.io/example-org/api-users@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
credential_ref = "ghcr-reader"
"#;

    let manifest: DeployManifest = toml::from_str(toml).unwrap();
    let artifact = manifest.artifact.unwrap();
    assert_eq!(
        artifact.reference.as_deref(),
        Some(
            "oci://ghcr.io/example-org/api-users@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        )
    );
    assert_eq!(artifact.credential_ref.as_deref(), Some("ghcr-reader"));
    assert!(artifact.url.is_empty());
    assert!(artifact.sha256.is_empty());
}

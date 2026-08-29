use super::{
    admin_tls_is_configured, admin_tls_material, artifact_server_url_is_loopback,
    bind_socket_address, build_artifact_server_url, build_proxy_advertised_address,
    clear_persisted_auth_override, collect_missing_node_stream_subscriptions,
    collect_unbacked_node_subscriptions, decode_vault_transit_hmac, load_kek_from_config,
    load_kek_from_env_spec, load_or_create_persisted_kek,
    load_or_create_persisted_secret_transport_keypair, load_passphrase_from_env_spec,
    load_secret_transport_keypair_from_config, sanitize_subject, serve_admin_app,
    subject_matches_filter, NODE_SUBSCRIPTION_SPECS, SEAL_KEY_DERIVATION_SALT_META_KEY,
    SECRET_TRANSPORT_KEY_META_KEY,
};
use common::auth::AuthConfig;
use common::config::{AdminSection, NodeConfig, ProxySection, RuntimeSection};
use common::types::{AppConfig, AppId, FuelQuota, GatewayRouteConfig, MemoryPages};
use messaging::events::Event;
use storage::Store;
use tempfile::{NamedTempFile, TempDir};

#[cfg(unix)]
fn shell_hex_key_command(hex_key: &str) -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-lc".to_string(),
        format!("printf '%s\\n' '{hex_key}'"),
    ]
}

fn spawn_mock_vault_kv_server(
    expected_token: &'static str,
    field_name: &'static str,
    field_value: &'static str,
) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let header_end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4)
                .unwrap();
            let header_text = String::from_utf8_lossy(&request[..header_end]).into_owned();
            let ok = header_text.lines().any(|line| {
                line.strip_prefix("x-vault-token:")
                    .or_else(|| line.strip_prefix("X-Vault-Token:"))
                    .map(str::trim)
                    == Some(expected_token)
            });
            let body = if ok {
                serde_json::json!({
                    "request_id": "test",
                    "data": {
                        "data": {
                            field_name: field_value
                        }
                    }
                })
                .to_string()
            } else {
                serde_json::json!({
                    "errors": ["forbidden"]
                })
                .to_string()
            };
            let status = if ok {
                "HTTP/1.1 200 OK"
            } else {
                "HTTP/1.1 403 Forbidden"
            };
            let response = format!(
                "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
    });
    (format!("http://{}", address), handle)
}

fn spawn_mock_vault_transit_server(
    expected_token: &'static str,
    expected_input: &'static str,
    hmac_hex: &'static str,
) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let header_end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4)
                .unwrap();
            let header_text = String::from_utf8_lossy(&request[..header_end]).into_owned();
            let content_length = header_text
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .or_else(|| line.strip_prefix("Content-Length:"))
                        .map(str::trim)
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let has_token = header_text.lines().any(|line| {
                line.strip_prefix("x-vault-token:")
                    .or_else(|| line.strip_prefix("X-Vault-Token:"))
                    .map(str::trim)
                    == Some(expected_token)
            });
            let body_json: Option<serde_json::Value> = std::str::from_utf8(&request[header_end..])
                .ok()
                .and_then(|body| serde_json::from_str(body).ok());
            let expected_input_value = expected_input
                .strip_prefix("\"input\":\"")
                .and_then(|value| value.strip_suffix('"'));
            let has_expected_input = expected_input_value.is_some_and(|expected| {
                body_json
                    .as_ref()
                    .and_then(|json| json.get("input"))
                    .and_then(|value| value.as_str())
                    == Some(expected)
            });
            let ok = has_token && has_expected_input;
            let body = if ok {
                serde_json::json!({
                    "data": {
                        "hmac": format!("vault:v1:{hmac_hex}")
                    }
                })
                .to_string()
            } else {
                serde_json::json!({
                    "errors": ["forbidden"]
                })
                .to_string()
            };
            let status = if ok {
                "HTTP/1.1 200 OK"
            } else {
                "HTTP/1.1 403 Forbidden"
            };
            let response = format!(
                "{status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
    });
    (format!("http://{}", address), handle)
}

fn spawn_mock_aws_kms_server(
    _expected_target: &'static str,
    expected_key_id: String,
    _expected_message_b64: String,
    mac_b64: String,
) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let header_end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4)
                .unwrap();
            let header_text = String::from_utf8_lossy(&request[..header_end]);
            let content_length = header_text
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .or_else(|| line.strip_prefix("Content-Length:"))
                        .map(str::trim)
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let _request_text = String::from_utf8_lossy(&request);
            let body = serde_json::json!({
                "KeyId": expected_key_id,
                "MacAlgorithm": "HMAC_SHA_256",
                "Mac": mac_b64
            })
            .to_string();
            let status = "HTTP/1.1 200 OK";
            let response = format!(
                "{status}\r\nContent-Type: application/x-amz-json-1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        }
    });
    (format!("http://{}", address), handle)
}

const TEST_ADMIN_TLS_CERT_PEM: &str = r#"-----BEGIN CERTIFICATE-----
MIIDJTCCAg2gAwIBAgIUAt2GkIIjTn/cu46520UjbQSS8FowDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDUxOTIxNTg0MFoXDTI2MDUy
MDIxNTg0MFowFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEA8lREjhEhJuV4ZePiYZZkXBpyqx+PypsyReF4J/PuXS2r
CCQtH557/sq7StoHWp0r1Qt7gzxyXd9A1UeVxjzj+GcUjiDdYcx/CcPdt+eUWm1v
IeIO6OZAiyADv89P1hK6L713gA9wbiNkmiGL+02u8/B6VAuCsZvgjfmT1R23K45V
/ofTkKEB7+HKrp8HBiKv0zENL8/+W2dRFFaPWIKNhTx1S71BE7dHhcr5t2zBYiyM
zwKEIvfz35Cby8DJLhKwLB9lAGeOn9b2VgqtUQIphp0FlwxdbK5MWJWd3Ogc0tb4
szvzNso5osiFrtCfgv7RroWMx0Mjzzd6RhGoF2SeqwIDAQABo28wbTAdBgNVHQ4E
FgQUOOyJajcQI9xXHGfGO3tYSWOtGoswHwYDVR0jBBgwFoAUOOyJajcQI9xXHGfG
O3tYSWOtGoswDwYDVR0TAQH/BAUwAwEB/zAaBgNVHREEEzARhwR/AAABgglsb2Nh
bGhvc3QwDQYJKoZIhvcNAQELBQADggEBAKwa3TXl7GWPoAOUErZwcExLzRBQuVji
mg11BI93QXSBtaD09GMeqx6D3y4j16gZLd5wZHBD6Whff5nm38WI1jyrKFwnNWI6
Nw205MZhbXmKxiROfLEFYIR2MwUTl5Ma6xR0szhEHYYSgLYlbS8Bobs5Z1wO0+Oy
khANpI5vxX9Ih85WYicQ1wL45L3iKx+E6HRBJBHJ71/d8s942lpGzyPyrX0j1orc
kbG/g6epb8tsUaLWYET9e8JkFaOxiZYy1DT2e1H//a2li5yox30JEgDDhmrRtlo/
cuA1MNK5uKiOJs4TZH38Cx7B2vlun/ZHEqCHwqb++CbSAhz7RBj+U10=
-----END CERTIFICATE-----
"#;

const TEST_ADMIN_TLS_KEY_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDyVESOESEm5Xhl
4+JhlmRcGnKrH4/KmzJF4Xgn8+5dLasIJC0fnnv+yrtK2gdanSvVC3uDPHJd30DV
R5XGPOP4ZxSOIN1hzH8Jw92355RabW8h4g7o5kCLIAO/z0/WErovvXeAD3BuI2Sa
IYv7Ta7z8HpUC4Kxm+CN+ZPVHbcrjlX+h9OQoQHv4cqunwcGIq/TMQ0vz/5bZ1EU
Vo9Ygo2FPHVLvUETt0eFyvm3bMFiLIzPAoQi9/PfkJvLwMkuErAsH2UAZ46f1vZW
Cq1RAimGnQWXDF1srkxYlZ3c6BzS1vizO/M2yjmiyIWu0J+C/tGuhYzHQyPPN3pG
EagXZJ6rAgMBAAECggEAIlhYKQx7duBSCprcRHmEutsSwnckMZKCcw4MMhlsAK/O
zEYYUSFssIV6OxcgsLKS+kx40nZYPT69mRzeuOx7YQL3ElfNGKXboX4tp/l9+L0G
4bYA5/huUGmWrnJK/evEkKyZScCmbi28/e1gQhtV/wPnyo6hFNwjXOvxDGT8R4NL
yanKMN+Dl0RqVPdA4tsucPBrVwOqSEjPVIn8NML8HbwbNyVrquV7Isc/4EP6zgjw
1QQBlCiHhAhkY9eGpthaQ85o7BVkGEHqUgdN8ysN56+hXorVxBpistTsTc/8mEy5
r8sMEOE4qx0OP2CPHqGyE0FSAmIGNNCJSKWi8L30uQKBgQD5YrnnV0k4v9dOV3D9
7MkHItnzMqBehlQAhMHyZ2S9HiLO24yoKlFvVEubwA/0gazVHzKaKdZIgeX662Av
suN2m681JLQqbM5ewRNyRde58r63krWuMNgAupEIb2x6piQN6PQtSY79zXAb1UyO
7scafaUjV61OZs/oM3EwuU7xOQKBgQD4waEyAP0nFIHkDkPTmaUHrKMbYfZp0/hw
iSZubQoASKqw6xEPFZn9LEqqjslR1KQS+EnFq7zDkAztPc+yrOsJN8iYoaKL9zna
7bx1HYVwrGWLfKZ+GCCwGTnQX8NrJ7AQoX9ajRrHhFLgpy5dMXVA5o+0wR+bwTaM
+5MDnWUjAwKBgEfMOKF168q+0InpespgRXAchIsT5D/ShJSxo/TZ95LK/lJ3uwMf
S9q1dh8dKHrIaq3hEXx41wyA+WlIIqUY54vaPpMaQhSExtVY2PRpTzZlwKqxPkUs
IsPy8pZvHdghxPeMPeBb8SL45nHc8vGjpQbnbYfDUk3kI69CQDA66ZNhAoGAf7tN
flOrqgmJuQTqJxlZ+FrZVhIzaZwCkiaaqVEsNYEaxMWveMNq0umPXYz8Kxy5M1Ry
7SGGSBULzjZTFDheZ9lRE67LvHsyJgy1HJ4QCw87BSj4hP72qfYKDclemwNCEQgc
UO7rtU9pDxpJYGkpAC5j1Djmdh/8VuBHWS/U4ukCgYEA6KDtUgXS2ztjuXzXEQjF
PBbvIkQLj/6muZ4ZgXThPwign1/5ih97ZBikWmmYB+zPme1gCj7otOMC9E6gxLmq
nWSPzrabXM5Z5hatTBeVxCQBoFL/hUTvOEqHXQuWtpqwHZ5PnTdsVk31sFJA0Vl3
uoKQp7o8ET+CcFRg9vEG/uA=
-----END PRIVATE KEY-----
"#;

static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn test_app_config(app_id: &str) -> AppConfig {
    AppConfig {
        id: AppId(app_id.to_string()),
        fuel_quota: FuelQuota(1_000_000),
        memory_limit: MemoryPages(4),
        max_instances: 1,
        idle_timeout_secs: 30,
        wasm_bind_port: 8080,
        env_vars: std::collections::HashMap::new(),
        secret_keys: vec![],
        extended_limits: None,
        health_check_path: None,
        db_max_connections: None,
        rate_limit: None,
        tenant_id: None,
        policy: None,
        namespace: "default".to_string(),
    }
}

#[test]
fn test_bind_socket_address_formats_ipv6() {
    let addr = bind_socket_address("::1", 9090).unwrap();
    assert_eq!(addr, "[::1]:9090");
}

#[test]
fn one_shot_auth_override_cleanup_removes_only_auth_record() {
    let temp = NamedTempFile::new().unwrap();
    let store = Store::open(temp.path()).unwrap();
    store.save_meta("unrelated", "preserved").unwrap();
    store
        .save_auth_config(&AuthConfig::generate_default())
        .unwrap();
    drop(store);

    clear_persisted_auth_override(temp.path().to_str().unwrap()).unwrap();

    let store = Store::open(temp.path()).unwrap();
    assert!(store.load_auth_config().unwrap().is_none());
    assert_eq!(
        store.load_meta("unrelated").unwrap().as_deref(),
        Some("preserved")
    );
}

#[test]
fn external_seal_key_rotation_rewraps_all_node_secret_material() {
    let temp = NamedTempFile::new().unwrap();
    let store = Store::open(temp.path()).unwrap();
    let old_seal_key = secrets::crypto::SymmetricKey::from_bytes([7; 32]);
    let new_seal_key = secrets::crypto::SymmetricKey::from_bytes([9; 32]);

    let original_kek = load_or_create_persisted_kek(&store, &old_seal_key, None).unwrap();
    let original_transport =
        load_or_create_persisted_secret_transport_keypair(&store, &old_seal_key, None).unwrap();

    let reloaded_kek =
        load_or_create_persisted_kek(&store, &new_seal_key, Some(&old_seal_key)).unwrap();
    let reloaded_transport = load_or_create_persisted_secret_transport_keypair(
        &store,
        &new_seal_key,
        Some(&old_seal_key),
    )
    .unwrap();
    assert_eq!(original_kek.as_bytes(), reloaded_kek.as_bytes());
    assert_eq!(
        original_transport.secret_bytes(),
        reloaded_transport.secret_bytes()
    );

    // The migration is durable: the previous key is no longer needed.
    assert_eq!(
        load_or_create_persisted_kek(&store, &new_seal_key, None)
            .unwrap()
            .as_bytes(),
        original_kek.as_bytes()
    );
    assert_eq!(
        load_or_create_persisted_secret_transport_keypair(&store, &new_seal_key, None)
            .unwrap()
            .secret_bytes(),
        original_transport.secret_bytes()
    );
}

#[test]
fn test_admin_tls_is_configured_requires_cert_and_key() {
    let mut config = NodeConfig::default();
    assert!(!admin_tls_is_configured(&config));

    config.proxy = ProxySection {
        tls_cert: Some("/tmp/admin.crt".to_string()),
        tls_key: Some("/tmp/admin.key".to_string()),
        ..ProxySection::default()
    };
    assert!(admin_tls_is_configured(&config));
}

#[test]
fn test_admin_tls_material_prefers_dedicated_admin_cert() {
    let mut config = NodeConfig {
        proxy: ProxySection {
            tls_cert: Some("/tmp/proxy.crt".to_string()),
            tls_key: Some("/tmp/proxy.key".to_string()),
            ..ProxySection::default()
        },
        ..NodeConfig::default()
    };
    config.admin.tls_cert = Some("/tmp/admin.crt".to_string());
    config.admin.tls_key = Some("/tmp/admin.key".to_string());

    let material = admin_tls_material(&config).expect("admin TLS material should exist");
    assert_eq!(material.0, "/tmp/admin.crt");
    assert_eq!(material.1, "/tmp/admin.key");
}

#[test]
fn test_startup_tls_requirement_rejects_auth_without_any_tls_material() {
    let mut config = NodeConfig::default();
    config.auth.enabled = true;
    config.auth.require_tls = true;
    config.auth.write_token = Some("valid_write_token_1234567890".to_string());

    let auth_config: AuthConfig = config.auth.clone().into();
    let err = proxy::auth_middleware::check_admin_tls_requirement(
        &auth_config,
        admin_tls_is_configured(&config),
    )
    .expect_err("auth.require_tls=true without TLS material should fail startup");

    assert!(err.contains("admin.tls_cert / admin.tls_key"));
}

#[test]
fn test_startup_tls_requirement_accepts_proxy_tls_fallback() {
    let mut config = NodeConfig::default();
    config.auth.enabled = true;
    config.auth.require_tls = true;
    config.auth.write_token = Some("valid_write_token_1234567890".to_string());
    config.proxy = ProxySection {
        tls_cert: Some("/tmp/proxy.crt".to_string()),
        tls_key: Some("/tmp/proxy.key".to_string()),
        ..ProxySection::default()
    };

    let auth_config: AuthConfig = config.auth.clone().into();
    proxy::auth_middleware::check_admin_tls_requirement(
        &auth_config,
        admin_tls_is_configured(&config),
    )
    .expect("shared proxy TLS material should satisfy admin auth TLS startup checks");
}

fn install_test_rustls_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn test_admin_app() -> axum::Router {
    axum::Router::new().route(
        "/ping",
        axum::routing::get(|| async { axum::Json(serde_json::json!({ "ok": true })) }),
    )
}

#[tokio::test]
async fn test_serve_admin_app_tls_accepts_https_requests() {
    install_test_rustls_provider();

    let temp_dir = TempDir::new().unwrap();
    let cert_path = temp_dir.path().join("admin.crt");
    let key_path = temp_dir.path().join("admin.key");
    std::fs::write(&cert_path, TEST_ADMIN_TLS_CERT_PEM).unwrap();
    std::fs::write(&key_path, TEST_ADMIN_TLS_KEY_PEM).unwrap();

    let probe_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = probe_listener.local_addr().unwrap().port();
    drop(probe_listener);

    let handle = tokio::spawn(serve_admin_app(
        format!("127.0.0.1:{port}"),
        test_admin_app(),
        Some(cert_path.to_string_lossy().to_string()),
        Some(key_path.to_string_lossy().to_string()),
    ));

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let mut response = None;
    for _ in 0..20 {
        match client
            .get(format!("https://127.0.0.1:{port}/ping"))
            .send()
            .await
        {
            Ok(resp) => {
                response = Some(resp);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }

    let response = response.expect("admin HTTPS listener did not respond in time");
    assert!(response.status().is_success());
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["ok"], true);

    handle.abort();
    let _ = handle.await;
}

#[tokio::test]
async fn test_serve_admin_app_tls_rejects_missing_cert_file() {
    install_test_rustls_provider();

    let err = serve_admin_app(
        "127.0.0.1:0".to_string(),
        test_admin_app(),
        Some("/tmp/does-not-exist-admin.crt".to_string()),
        Some("/tmp/does-not-exist-admin.key".to_string()),
    )
    .await
    .expect_err("missing TLS files should fail");

    assert!(err.to_string().contains("admin TLS config error"));
}

#[tokio::test]
async fn test_serve_admin_app_tls_rejects_invalid_pem_contents() {
    install_test_rustls_provider();

    let temp_dir = TempDir::new().unwrap();
    let cert_path = temp_dir.path().join("bad-admin.crt");
    let key_path = temp_dir.path().join("bad-admin.key");
    std::fs::write(&cert_path, b"not a cert").unwrap();
    std::fs::write(&key_path, b"not a key").unwrap();

    let err = serve_admin_app(
        "127.0.0.1:0".to_string(),
        test_admin_app(),
        Some(cert_path.to_string_lossy().to_string()),
        Some(key_path.to_string_lossy().to_string()),
    )
    .await
    .expect_err("invalid TLS PEM should fail");

    assert!(err.to_string().contains("admin TLS config error"));
}

#[test]
fn test_build_artifact_server_url_defaults_to_loopback() {
    let admin = AdminSection::default();
    let url = build_artifact_server_url(&admin).unwrap();
    assert_eq!(url, "http://127.0.0.1:9091");
    assert!(artifact_server_url_is_loopback(&url));
}

#[test]
fn test_build_artifact_server_url_from_advertised_host() {
    let admin = AdminSection {
        advertised_host: Some("node-1.internal".to_string()),
        ..AdminSection::default()
    };
    let url = build_artifact_server_url(&admin).unwrap();
    assert_eq!(url, "http://node-1.internal:9091");
    assert!(!artifact_server_url_is_loopback(&url));
}

#[test]
fn test_build_artifact_server_url_from_explicit_url() {
    let admin = AdminSection {
        advertised_artifact_url: Some("https://artifacts.node-1.internal/base/".to_string()),
        ..AdminSection::default()
    };
    let url = build_artifact_server_url(&admin).unwrap();
    assert_eq!(url, "https://artifacts.node-1.internal/base");
    assert!(!artifact_server_url_is_loopback(&url));
}

#[test]
fn test_subscription_matrix_covers_each_event_type_exactly_once() {
    let route = common::types::Route {
        host: "example.com".to_string(),
        path_prefix: "/".to_string(),
        app_id: AppId("demo:v1".to_string()),
        strip_prefix: false,
        created_at: 0,
        updated_at: 0,
    };
    let gateway_config = GatewayRouteConfig::default();
    let config = test_app_config("demo:v1");
    let event_cases = vec![
        (
            "deploy_app",
            "DEPLOY",
            Event::DeployApp {
                app_id: config.id.clone(),
                config: config.clone(),
                artifact_url: "http://node.internal/artifacts/abc".to_string(),
                artifact_transfer_manifests: vec![],
                expected_hash: Some("abc".to_string()),
                size_bytes: 123,
            },
        ),
        (
            "remove_app",
            "DEPLOY",
            Event::RemoveApp {
                app_id: config.id.clone(),
            },
        ),
        (
            "route_add",
            "DEPLOY",
            Event::RouteAdd {
                route: route.clone(),
            },
        ),
        (
            "route_remove",
            "DEPLOY",
            Event::RouteRemove {
                host: route.host.clone(),
            },
        ),
        (
            "instance_ready",
            "CONTROL",
            Event::InstanceReady {
                app_id: config.id.clone(),
                addr: "127.0.0.1:18080".parse().unwrap(),
                node_id: "node-a".to_string(),
            },
        ),
        (
            "instance_dead",
            "CONTROL",
            Event::InstanceDead {
                app_id: config.id.clone(),
                addr: "127.0.0.1:18080".parse().unwrap(),
                node_id: "node-a".to_string(),
            },
        ),
        (
            "secret_update",
            "CONTROL",
            Event::SecretUpdate {
                app_id: config.id.clone(),
                key: "API_KEY".to_string(),
                target_node_id: None,
                secret: secrets::SecretTransportEnvelope::plaintext_utf8("value"),
            },
        ),
        (
            "config_update",
            "CONTROL",
            Event::ConfigUpdate {
                app_id: config.id.clone(),
                config: config.clone(),
            },
        ),
        (
            "gateway_config_update",
            "CONTROL",
            Event::GatewayConfigUpdate {
                app_id: config.id.clone(),
                config: gateway_config.clone(),
            },
        ),
        (
            "gateway_config_remove",
            "CONTROL",
            Event::GatewayConfigRemove {
                app_id: config.id.clone(),
            },
        ),
        (
            "node_load",
            "NODE",
            Event::NodeLoad {
                node_id: "node-a".to_string(),
                cpu_percent: 10.0,
                fuel_budget_used_percent: 20.0,
                active_instances: 1,
                proxy_address: "node-a.internal:8080".to_string(),
            },
        ),
        (
            "node_joined",
            "NODE",
            Event::NodeJoined {
                node_id: "node-b".to_string(),
                bootstrap_session_id: "session-1".to_string(),
                bootstrap_nonce: "nonce-1".to_string(),
                artifact_server_url: "http://node-b.internal:9091".to_string(),
                public_key_bytes: vec![7u8; 32],
                protocol_version: common::protocol::PROTOCOL_VERSION,
                binary_version: common::protocol::BINARY_VERSION.to_string(),
            },
        ),
        (
            "state_snapshot",
            "NODE",
            Event::StateSnapshot {
                for_node_id: "node-b".to_string(),
                bootstrap_session_id: "session-1".to_string(),
                bootstrap_nonce: "nonce-1".to_string(),
                configs: vec![],
                routes: vec![],
                encrypted_secrets: vec![],
                gateway_configs: vec![],
                api_keys: vec![],
                artifact_fetches: vec![],
                artifact_hashes: vec![],
            },
        ),
        (
            "health_changed",
            "HEALTH",
            Event::NodeHealthChanged {
                node_id: "node-a".to_string(),
                status: "healthy".to_string(),
                cause: None,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                active_instances: 1,
                accepting_requests: true,
            },
        ),
        (
            "health_snapshot",
            "HEALTH",
            Event::NodeHealthSnapshot {
                node_id: "node-a".to_string(),
                status: "healthy".to_string(),
                active_instances: 1,
                deployed_apps: 1,
                nats_connected: true,
                disk_free_mb: 100,
                memory_used_mb: 50,
                timestamp: "2026-01-01T00:00:00Z".to_string(),
            },
        ),
        (
            "node_upgrade_targeted",
            "PLATFORM",
            Event::NodeUpgrade {
                target_node: "node-a".to_string(),
                binary_url: "https://example.com/node".to_string(),
                binary_sha256: "deadbeef".to_string(),
                signature_ed25519: None,
                release_provenance: None,
                new_protocol_version: common::protocol::PROTOCOL_VERSION,
                new_binary_version: "1.2.3".to_string(),
            },
        ),
        (
            "node_upgrade_rolling",
            "PLATFORM",
            Event::NodeUpgrade {
                target_node: "*".to_string(),
                binary_url: "https://example.com/node".to_string(),
                binary_sha256: "deadbeef".to_string(),
                signature_ed25519: None,
                release_provenance: None,
                new_protocol_version: common::protocol::PROTOCOL_VERSION,
                new_binary_version: "1.2.3".to_string(),
            },
        ),
        (
            "node_upgrade_complete",
            "PLATFORM",
            Event::NodeUpgradeComplete {
                node_id: "node-a".to_string(),
                new_binary_version: "1.2.3".to_string(),
                new_protocol_version: common::protocol::PROTOCOL_VERSION,
            },
        ),
        (
            "node_draining",
            "PLATFORM",
            Event::NodeDraining {
                node_id: "node-a".to_string(),
                drain_timeout_secs: 30,
            },
        ),
        (
            "config_hot_reload",
            "PLATFORM",
            Event::ConfigHotReload {
                node_id: "node-a".to_string(),
                changes: serde_json::json!({"health": {"check_interval_secs": 5}}),
            },
        ),
        (
            "node_under_pressure",
            "EBPF",
            Event::NodeUnderPressure {
                node_id: "node-a".to_string(),
                pressure_level: 2,
            },
        ),
        (
            "node_pressure_recovered",
            "EBPF",
            Event::NodePressureRecovered {
                node_id: "node-a".to_string(),
            },
        ),
        (
            "security_incident",
            "EBPF",
            Event::SecurityIncident {
                node_id: "node-a".to_string(),
                app_id: config.id.0.clone(),
                pid: 42,
                syscall_nr: 31337,
                category: "PrivilegeEscalation".to_string(),
            },
        ),
    ];

    for (label, expected_stream, event) in event_cases {
        let subject = event.subject();
        let matches: Vec<_> = NODE_SUBSCRIPTION_SPECS
            .iter()
            .copied()
            .filter(|(_, filter)| subject_matches_filter(&subject, filter))
            .collect();

        assert_eq!(
            matches.len(),
            1,
            "{label} with subject {subject} should map to exactly one subscription filter, got {matches:?}"
        );
        assert_eq!(
            matches[0].0, expected_stream,
            "{label} with subject {subject} mapped to wrong stream/filter {:?}",
            matches[0]
        );
    }
}

#[test]
fn test_sanitize_subject_stabilizes_consumer_suffixes() {
    assert_eq!(sanitize_subject("gateway.config.>"), "gateway-config-all");
    assert_eq!(sanitize_subject("ebpf.pressure.*"), "ebpf-pressure-one");
}

#[test]
fn test_node_subscription_specs_cover_all_declared_stream_subjects() {
    let missing = collect_missing_node_stream_subscriptions();
    assert!(
        missing.is_empty(),
        "declared JetStream subjects without node subscriptions: {missing:?}"
    );
}

#[test]
fn test_node_subscription_specs_only_reference_declared_stream_subjects() {
    let unbacked = collect_unbacked_node_subscriptions();
    assert!(
        unbacked.is_empty(),
        "node subscriptions without declared JetStream stream subjects: {unbacked:?}"
    );
}

#[test]
fn test_build_proxy_advertised_address_defaults_to_loopback() {
    let config = NodeConfig::default();
    let addr = build_proxy_advertised_address(&config).unwrap();
    assert_eq!(addr, "127.0.0.1:8080");
}

#[test]
fn test_build_proxy_advertised_address_uses_advertised_host() {
    let mut config = NodeConfig::default();
    config.admin.advertised_host = Some("node-1.internal".to_string());
    config.proxy.http_port = 18080;

    let addr = build_proxy_advertised_address(&config).unwrap();
    assert_eq!(addr, "node-1.internal:18080");
}

#[test]
fn test_load_kek_from_env_spec_hex() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    let var_name = "WASM_NODE_TEST_KEK_HEX";
    let value = "11".repeat(32);
    std::env::set_var(var_name, &value);

    let key = load_kek_from_env_spec(&format!("env:{var_name}")).unwrap();
    assert_eq!(key.as_bytes(), &[0x11; 32]);

    std::env::remove_var(var_name);
}

#[test]
fn test_load_kek_from_env_spec_raw_32_bytes() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    let var_name = "WASM_NODE_TEST_KEK_RAW";
    let value = "A".repeat(32);
    std::env::set_var(var_name, &value);

    let key = load_kek_from_env_spec(&format!("env:{var_name}")).unwrap();
    assert_eq!(key.as_bytes(), value.as_bytes());

    std::env::remove_var(var_name);
}

#[test]
fn test_load_passphrase_from_env_spec_rejects_empty_value() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    let var_name = "WASM_NODE_TEST_EMPTY_PASSPHRASE";
    std::env::set_var(var_name, "   ");
    let err = load_passphrase_from_env_spec(&format!("passphrase-env:{var_name}")).unwrap_err();
    assert!(err.to_string().contains("must not be empty"));
    std::env::remove_var(var_name);
}

#[test]
fn test_file_key_source_initializes_sealed_kek_from_key_file() {
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();

    let temp_dir = TempDir::new().unwrap();
    let key_path = temp_dir.path().join("master.key");
    let seal_key = [0x22u8; 32];
    std::fs::write(&key_path, seal_key).unwrap();

    let runtime = RuntimeSection {
        key_source: "file".to_string(),
        key_file: Some(key_path.to_string_lossy().to_string()),
        ..Default::default()
    };

    let key = load_kek_from_config(&store, &runtime).unwrap();
    assert_eq!(key.as_bytes(), &seal_key);

    let persisted = store.load_kek().unwrap().unwrap();
    assert_ne!(persisted, seal_key.to_vec());
    assert!(persisted.len() > 32);
}

#[test]
fn test_file_key_source_reloads_existing_sealed_kek() {
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();

    let temp_dir = TempDir::new().unwrap();
    let key_path = temp_dir.path().join("master.key");
    let seal_key = [0x44u8; 32];
    std::fs::write(&key_path, seal_key).unwrap();

    let runtime = RuntimeSection {
        key_source: "file".to_string(),
        key_file: Some(key_path.to_string_lossy().to_string()),
        ..Default::default()
    };

    let first = load_kek_from_config(&store, &runtime).unwrap();
    let second = load_kek_from_config(&store, &runtime).unwrap();
    assert_eq!(first.as_bytes(), second.as_bytes());
}

#[test]
fn test_file_key_source_initializes_and_reloads_secret_transport_keypair() {
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();

    let temp_dir = TempDir::new().unwrap();
    let key_path = temp_dir.path().join("master.key");
    let seal_key = [0x77u8; 32];
    std::fs::write(&key_path, seal_key).unwrap();

    let runtime = RuntimeSection {
        key_source: "file".to_string(),
        key_file: Some(key_path.to_string_lossy().to_string()),
        ..Default::default()
    };

    let first = load_secret_transport_keypair_from_config(&store, &runtime).unwrap();
    let second = load_secret_transport_keypair_from_config(&store, &runtime).unwrap();

    assert_eq!(first.public_bytes(), second.public_bytes());
    let persisted = store
        .load_meta(SECRET_TRANSPORT_KEY_META_KEY)
        .unwrap()
        .unwrap();
    assert!(!persisted.is_empty());
}

#[cfg(unix)]
#[test]
fn test_command_key_source_initializes_and_reloads_sealed_kek() {
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();

    let runtime = RuntimeSection {
        key_source: "command".to_string(),
        key_command: shell_hex_key_command(
            "1111111111111111111111111111111111111111111111111111111111111111",
        ),
        ..Default::default()
    };

    let first = load_kek_from_config(&store, &runtime).unwrap();
    let second = load_kek_from_config(&store, &runtime).unwrap();
    assert_eq!(first.as_bytes(), second.as_bytes());

    let persisted = store.load_kek().unwrap().unwrap();
    assert!(persisted.len() > 32);
}

#[cfg(unix)]
#[test]
fn test_command_key_source_initializes_and_reloads_secret_transport_keypair() {
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();

    let runtime = RuntimeSection {
        key_source: "command".to_string(),
        key_command: shell_hex_key_command(
            "2222222222222222222222222222222222222222222222222222222222222222",
        ),
        ..Default::default()
    };

    let first = load_secret_transport_keypair_from_config(&store, &runtime).unwrap();
    let second = load_secret_transport_keypair_from_config(&store, &runtime).unwrap();
    assert_eq!(first.public_bytes(), second.public_bytes());
}

#[cfg(unix)]
#[test]
fn test_command_key_source_rejects_nonzero_exit() {
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();

    let runtime = RuntimeSection {
        key_source: "command".to_string(),
        key_command: vec![
            "/bin/sh".to_string(),
            "-lc".to_string(),
            "echo boom >&2; exit 7".to_string(),
        ],
        ..Default::default()
    };

    let err = match load_kek_from_config(&store, &runtime) {
        Ok(_) => panic!("expected command key source failure"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("failed with status 7"));
}

#[test]
fn test_vault_kv_key_source_initializes_and_reloads_sealed_kek() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();
    let token_env = "WASM_NODE_TEST_VAULT_TOKEN";
    std::env::set_var(token_env, "vault-token-123");
    let (vault_url, server) = spawn_mock_vault_kv_server(
        "vault-token-123",
        "key",
        "3333333333333333333333333333333333333333333333333333333333333333",
    );

    let runtime = RuntimeSection {
        key_source: "vault-kv".to_string(),
        key_vault_url: Some(vault_url),
        key_vault_token_env: Some(token_env.to_string()),
        key_vault_mount: "secret".to_string(),
        key_vault_path: Some("wasm-node/seal-key".to_string()),
        key_vault_field: "key".to_string(),
        ..Default::default()
    };

    let first = load_kek_from_config(&store, &runtime).unwrap();
    let second = load_kek_from_config(&store, &runtime).unwrap();
    assert_eq!(first.as_bytes(), second.as_bytes());
    assert!(store.load_kek().unwrap().unwrap().len() > 32);

    server.join().unwrap();
    std::env::remove_var(token_env);
}

#[test]
fn test_vault_kv_key_source_initializes_and_reloads_secret_transport_keypair() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();
    let token_env = "WASM_NODE_TEST_VAULT_TRANSPORT_TOKEN";
    std::env::set_var(token_env, "vault-token-456");
    let (vault_url, server) = spawn_mock_vault_kv_server(
        "vault-token-456",
        "key",
        "4444444444444444444444444444444444444444444444444444444444444444",
    );

    let runtime = RuntimeSection {
        key_source: "vault-kv".to_string(),
        key_vault_url: Some(vault_url),
        key_vault_token_env: Some(token_env.to_string()),
        key_vault_mount: "secret".to_string(),
        key_vault_path: Some("wasm-node/transport-key".to_string()),
        key_vault_field: "key".to_string(),
        ..Default::default()
    };

    let first = load_secret_transport_keypair_from_config(&store, &runtime).unwrap();
    let second = load_secret_transport_keypair_from_config(&store, &runtime).unwrap();
    assert_eq!(first.public_bytes(), second.public_bytes());

    server.join().unwrap();
    std::env::remove_var(token_env);
}

#[test]
fn test_vault_transit_key_source_initializes_and_reloads_sealed_kek() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();
    let token_env = "WASM_NODE_TEST_VAULT_TRANSIT_TOKEN";
    std::env::set_var(token_env, "vault-transit-token-123");
    let (vault_url, server) = spawn_mock_vault_transit_server(
        "vault-transit-token-123",
        "\"input\":\"cHJvZC1ub2RlLTA=\"",
        "5555555555555555555555555555555555555555555555555555555555555555",
    );

    let runtime = RuntimeSection {
        key_source: "vault-transit".to_string(),
        key_vault_url: Some(vault_url),
        key_vault_token_env: Some(token_env.to_string()),
        key_vault_transit_mount: "transit".to_string(),
        key_vault_transit_key: Some("wasm-node-seal".to_string()),
        key_vault_transit_context: Some("prod-node-0".to_string()),
        ..Default::default()
    };

    let first = load_kek_from_config(&store, &runtime).unwrap();
    let second = load_kek_from_config(&store, &runtime).unwrap();
    assert_eq!(first.as_bytes(), second.as_bytes());
    assert!(store.load_kek().unwrap().unwrap().len() > 32);

    server.join().unwrap();
    std::env::remove_var(token_env);
}

#[test]
fn test_decode_vault_transit_hmac_accepts_real_base64_and_legacy_hex() {
    use base64::Engine as _;

    let expected = [0x5au8; 32];
    let encoded = base64::engine::general_purpose::STANDARD.encode(expected);
    assert_eq!(
        decode_vault_transit_hmac(&format!("vault:v7:{encoded}"))
            .expect("real Vault base64 HMAC should decode"),
        expected
    );
    assert_eq!(
        decode_vault_transit_hmac(&format!("vault:v3:{}", hex::encode(expected)))
            .expect("legacy mock hex HMAC should decode"),
        expected
    );
}

#[test]
fn test_decode_vault_transit_hmac_rejects_invalid_or_wrong_length_data() {
    assert!(decode_vault_transit_hmac("vault:v1:not-base64").is_err());
    assert!(decode_vault_transit_hmac("vault:v1:YQ==").is_err());
}

#[test]
fn test_vault_transit_key_source_initializes_and_reloads_secret_transport_keypair() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();
    let token_env = "WASM_NODE_TEST_VAULT_TRANSIT_TRANSPORT_TOKEN";
    std::env::set_var(token_env, "vault-transit-token-456");
    let (vault_url, server) = spawn_mock_vault_transit_server(
        "vault-transit-token-456",
        "\"input\":\"cHJvZC1ub2RlLTE=\"",
        "6666666666666666666666666666666666666666666666666666666666666666",
    );

    let runtime = RuntimeSection {
        key_source: "vault-transit".to_string(),
        key_vault_url: Some(vault_url),
        key_vault_token_env: Some(token_env.to_string()),
        key_vault_transit_mount: "transit".to_string(),
        key_vault_transit_key: Some("wasm-node-seal".to_string()),
        key_vault_transit_context: Some("prod-node-1".to_string()),
        ..Default::default()
    };

    let first = load_secret_transport_keypair_from_config(&store, &runtime).unwrap();
    let second = load_secret_transport_keypair_from_config(&store, &runtime).unwrap();
    assert_eq!(first.public_bytes(), second.public_bytes());

    server.join().unwrap();
    std::env::remove_var(token_env);
}

#[test]
fn test_aws_kms_hmac_key_source_initializes_and_reloads_sealed_kek() {
    use base64::Engine as _;

    let _guard = ENV_TEST_LOCK.lock().unwrap();
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();
    std::env::set_var("AWS_ACCESS_KEY_ID", "test");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
    std::env::set_var("AWS_EC2_METADATA_DISABLED", "true");
    let context = "prod-node-0";
    let message_b64 = base64::engine::general_purpose::STANDARD.encode(context.as_bytes());
    let mac_b64 = base64::engine::general_purpose::STANDARD.encode([0x77u8; 32]);
    let key_id = "arn:aws:kms:eu-west-3:123456789012:key/test";
    let (endpoint, server) = spawn_mock_aws_kms_server(
        "TrentService.GenerateMac",
        key_id.to_string(),
        message_b64,
        mac_b64,
    );

    let runtime = RuntimeSection {
        key_source: "aws-kms-hmac".to_string(),
        key_aws_kms_region: Some("eu-west-3".to_string()),
        key_aws_kms_endpoint: Some(endpoint),
        key_aws_kms_key_id: Some(key_id.to_string()),
        key_aws_kms_context: Some(context.to_string()),
        ..Default::default()
    };

    let first = load_kek_from_config(&store, &runtime).unwrap();
    let second = load_kek_from_config(&store, &runtime).unwrap();
    assert_eq!(first.as_bytes(), second.as_bytes());
    assert!(store.load_kek().unwrap().unwrap().len() > 32);

    server.join().unwrap();
    std::env::remove_var("AWS_ACCESS_KEY_ID");
    std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    std::env::remove_var("AWS_EC2_METADATA_DISABLED");
}

#[test]
fn test_aws_kms_hmac_key_source_initializes_and_reloads_secret_transport_keypair() {
    use base64::Engine as _;

    let _guard = ENV_TEST_LOCK.lock().unwrap();
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();
    std::env::set_var("AWS_ACCESS_KEY_ID", "test");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
    std::env::set_var("AWS_EC2_METADATA_DISABLED", "true");
    let context = "prod-node-1";
    let message_b64 = base64::engine::general_purpose::STANDARD.encode(context.as_bytes());
    let mac_b64 = base64::engine::general_purpose::STANDARD.encode([0x88u8; 32]);
    let key_id = "arn:aws:kms:eu-west-3:123456789012:key/test";
    let (endpoint, server) = spawn_mock_aws_kms_server(
        "TrentService.GenerateMac",
        key_id.to_string(),
        message_b64,
        mac_b64,
    );

    let runtime = RuntimeSection {
        key_source: "aws-kms-hmac".to_string(),
        key_aws_kms_region: Some("eu-west-3".to_string()),
        key_aws_kms_endpoint: Some(endpoint),
        key_aws_kms_key_id: Some(key_id.to_string()),
        key_aws_kms_context: Some(context.to_string()),
        ..Default::default()
    };

    let first = load_secret_transport_keypair_from_config(&store, &runtime).unwrap();
    let second = load_secret_transport_keypair_from_config(&store, &runtime).unwrap();
    assert_eq!(first.public_bytes(), second.public_bytes());

    server.join().unwrap();
    std::env::remove_var("AWS_ACCESS_KEY_ID");
    std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    std::env::remove_var("AWS_EC2_METADATA_DISABLED");
}

#[test]
fn test_passphrase_env_key_source_initializes_and_reloads_sealed_kek() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();

    let var_name = "WASM_NODE_TEST_PASSPHRASE";
    std::env::set_var(var_name, "correct horse battery staple");
    let runtime = RuntimeSection {
        key_source: format!("passphrase-env:{var_name}"),
        key_file: None,
        ..Default::default()
    };

    let first = load_kek_from_config(&store, &runtime).unwrap();
    let second = load_kek_from_config(&store, &runtime).unwrap();
    assert_eq!(first.as_bytes(), second.as_bytes());

    let persisted = store.load_kek().unwrap().unwrap();
    assert!(persisted.len() > 32);
    let salt = store
        .load_meta(SEAL_KEY_DERIVATION_SALT_META_KEY)
        .unwrap()
        .unwrap();
    assert!(!salt.is_empty());

    std::env::remove_var(var_name);
}

#[test]
fn test_passphrase_env_key_source_initializes_and_reloads_secret_transport_keypair() {
    let _guard = ENV_TEST_LOCK.lock().unwrap();
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();

    let var_name = "WASM_NODE_TEST_TRANSPORT_PASSPHRASE";
    std::env::set_var(var_name, "node transport passphrase");
    let runtime = RuntimeSection {
        key_source: format!("passphrase-env:{var_name}"),
        key_file: None,
        ..Default::default()
    };

    let first = load_secret_transport_keypair_from_config(&store, &runtime).unwrap();
    let second = load_secret_transport_keypair_from_config(&store, &runtime).unwrap();
    assert_eq!(first.public_bytes(), second.public_bytes());

    std::env::remove_var(var_name);
}

#[test]
fn test_file_key_source_migrates_legacy_plaintext_db_kek_into_sealed_blob() {
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();
    let legacy = [0x55u8; 32];
    store.save_kek(&legacy).unwrap();

    let temp_dir = TempDir::new().unwrap();
    let key_path = temp_dir.path().join("master.key");
    std::fs::write(&key_path, [0x66u8; 32]).unwrap();
    let runtime = RuntimeSection {
        key_source: "file".to_string(),
        key_file: Some(key_path.to_string_lossy().to_string()),
        ..Default::default()
    };

    let key = load_kek_from_config(&store, &runtime).unwrap();
    assert_eq!(key.as_bytes(), &legacy);
    let persisted = store.load_kek().unwrap().unwrap();
    assert_ne!(persisted, legacy.to_vec());
    assert!(persisted.len() > 32);
}

#[test]
fn test_wrong_file_seal_key_rejects_sealed_kek() {
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();

    let temp_dir = TempDir::new().unwrap();
    let key_path = temp_dir.path().join("master.key");
    std::fs::write(&key_path, [0x77u8; 32]).unwrap();
    let runtime = RuntimeSection {
        key_source: "file".to_string(),
        key_file: Some(key_path.to_string_lossy().to_string()),
        ..Default::default()
    };
    let _ = load_kek_from_config(&store, &runtime).unwrap();

    std::fs::write(&key_path, [0x88u8; 32]).unwrap();
    assert!(load_kek_from_config(&store, &runtime).is_err());
}

#[test]
fn test_generate_key_source_rejects_persisted_kek() {
    let temp_db = NamedTempFile::new().unwrap();
    let store = Store::open(temp_db.path()).unwrap();
    store.save_kek(&[0x33u8; 48]).unwrap();

    let runtime = RuntimeSection {
        key_source: "generate".to_string(),
        ..Default::default()
    };

    let err = match load_kek_from_config(&store, &runtime) {
        Ok(_) => panic!("expected persisted KEK rejection"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("persisted KEK detected"));
}

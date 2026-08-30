use clap::Parser;
use messaging::reconnect::{NatsHealth, NatsHealthWatcher};
use reqwest::Url;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

fn symm_key_from_exact_32(
    bytes: &[u8],
    source: &str,
) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    if bytes.len() != 32 {
        anyhow::bail!(
            "{source} must contain exactly 32 bytes, found {} bytes",
            bytes.len()
        );
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(bytes);
    Ok(secrets::crypto::SymmetricKey::from_bytes(key))
}

fn build_vault_agent(runtime: &common::config::RuntimeSection) -> anyhow::Result<ureq::Agent> {
    let mut builder =
        ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(5)));
    if let Some(ca_path) = runtime
        .key_vault_ca_cert
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    {
        let pem = std::fs::read(ca_path)
            .map_err(|e| anyhow::anyhow!("failed to read Vault CA bundle {ca_path}: {e}"))?;
        let mut certificates = Vec::new();
        for item in ureq::tls::parse_pem(&pem) {
            match item
                .map_err(|e| anyhow::anyhow!("failed to parse Vault CA bundle {ca_path}: {e}"))?
            {
                ureq::tls::PemItem::Certificate(certificate) => certificates.push(certificate),
                ureq::tls::PemItem::PrivateKey(_) => {}
                _ => {}
            }
        }
        if certificates.is_empty() {
            anyhow::bail!("Vault CA bundle {ca_path} contains no certificates");
        }
        builder = builder.tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::new_with_certs(&certificates))
                .build(),
        );
    }
    Ok(builder.build().into())
}

fn decode_vault_transit_hmac(hmac: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine as _;

    let encoded = hmac
        .rsplit(':')
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Vault transit hmac response is empty"))?;
    let decoded = if encoded.len() == 64 && encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        hex::decode(encoded)
            .map_err(|e| anyhow::anyhow!("failed to decode legacy hex Vault transit hmac: {e}"))?
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| anyhow::anyhow!("failed to decode Vault transit hmac base64: {e}"))?
    };
    if decoded.len() != 32 {
        anyhow::bail!(
            "Vault transit hmac must decode to exactly 32 bytes, found {} bytes",
            decoded.len()
        );
    }
    Ok(decoded)
}

fn load_kek_from_env_spec(spec: &str) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    let var_name = spec
        .strip_prefix("env:")
        .ok_or_else(|| anyhow::anyhow!("invalid env key source: {spec}"))?;
    let raw = std::env::var(var_name)
        .map_err(|_| anyhow::anyhow!("environment variable {var_name} is not set"))?;
    let trimmed = raw.trim();

    // Accept either raw 32-byte strings or 64-char hex for operator convenience.
    if trimmed.len() == 64 {
        let decoded = hex::decode(trimmed)
            .map_err(|e| anyhow::anyhow!("failed to decode hex KEK from {var_name}: {e}"))?;
        return symm_key_from_exact_32(&decoded, &format!("environment variable {var_name}"));
    }

    symm_key_from_exact_32(raw.as_bytes(), &format!("environment variable {var_name}"))
}

fn load_passphrase_from_env_spec(spec: &str) -> anyhow::Result<String> {
    let var_name = spec
        .strip_prefix("passphrase-env:")
        .ok_or_else(|| anyhow::anyhow!("invalid passphrase env key source: {spec}"))?;
    let raw = std::env::var(var_name)
        .map_err(|_| anyhow::anyhow!("environment variable {var_name} is not set"))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("environment variable {var_name} must not be empty");
    }
    Ok(raw)
}

fn load_kek_from_command(command: &[String]) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    if command.is_empty() {
        anyhow::bail!("runtime.key_source=command requires runtime.key_command");
    }
    let mut process = std::process::Command::new(&command[0]);
    if command.len() > 1 {
        process.args(&command[1..]);
    }
    let output = process
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run key command {}: {e}", command[0]))?;
    if !output.status.success() {
        anyhow::bail!(
            "key command {} failed with status {}; stderr suppressed because secret-manager helpers may emit sensitive material",
            command[0],
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "terminated by signal".to_string())
        );
    }

    if output.stdout.len() == 32 {
        tracing::info!(command = %command[0], "loaded KEK seal key from command output");
        return symm_key_from_exact_32(&output.stdout, &format!("key command {}", command[0]));
    }

    let stdout = String::from_utf8(output.stdout).map_err(|e| {
        anyhow::anyhow!("key command {} produced non-UTF-8 output: {e}", command[0])
    })?;
    let trimmed = stdout.trim();
    if trimmed.len() == 64 {
        let decoded = hex::decode(trimmed).map_err(|e| {
            anyhow::anyhow!(
                "failed to decode hex KEK from key command {}: {e}",
                command[0]
            )
        })?;
        tracing::info!(command = %command[0], "loaded KEK seal key from hex command output");
        return symm_key_from_exact_32(&decoded, &format!("key command {}", command[0]));
    }

    symm_key_from_exact_32(trimmed.as_bytes(), &format!("key command {}", command[0]))
}

fn load_kek_from_vault_kv(
    runtime: &common::config::RuntimeSection,
) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    let url = runtime
        .key_vault_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("runtime.key_source=vault-kv requires runtime.key_vault_url")
        })?;
    let token_env = runtime
        .key_vault_token_env
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("runtime.key_source=vault-kv requires runtime.key_vault_token_env")
        })?;
    let secret_path = runtime
        .key_vault_path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("runtime.key_source=vault-kv requires runtime.key_vault_path")
        })?;
    let token = std::env::var(token_env)
        .map_err(|_| anyhow::anyhow!("environment variable {token_env} is not set"))?;
    let mount = runtime.key_vault_mount.trim();
    let field = runtime.key_vault_field.trim();
    let request_url = format!(
        "{}/v1/{}/data/{}",
        url.trim_end_matches('/'),
        mount,
        secret_path.trim_start_matches('/')
    );
    let agent = build_vault_agent(runtime)?;
    let mut response = agent
        .get(&request_url)
        .header("X-Vault-Token", token.trim())
        .call()
        .map_err(|e| anyhow::anyhow!("failed to fetch seal key from Vault KV: {e}"))?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| anyhow::anyhow!("failed to read Vault KV response body: {e}"))?;
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("failed to parse Vault KV response JSON: {e}"))?;
    let key_value = json
        .get("data")
        .and_then(|value| value.get("data"))
        .and_then(|value| value.get(field))
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Vault KV response did not contain string field '{}' under data.data",
                field
            )
        })?;
    if key_value.len() == 64 {
        let decoded = hex::decode(key_value).map_err(|e| {
            anyhow::anyhow!("failed to decode hex KEK from Vault KV field {field}: {e}")
        })?;
        tracing::info!(
            mount = mount,
            path = secret_path,
            field = field,
            "loaded KEK seal key from Vault KV"
        );
        return symm_key_from_exact_32(&decoded, &format!("Vault KV field {field}"));
    }
    tracing::info!(
        mount = mount,
        path = secret_path,
        field = field,
        "loaded KEK seal key from Vault KV"
    );
    symm_key_from_exact_32(key_value.as_bytes(), &format!("Vault KV field {field}"))
}

fn load_kek_from_vault_transit(
    runtime: &common::config::RuntimeSection,
) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    let url = runtime
        .key_vault_url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("runtime.key_source=vault-transit requires runtime.key_vault_url")
        })?;
    let token_env = runtime
        .key_vault_token_env
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("runtime.key_source=vault-transit requires runtime.key_vault_token_env")
        })?;
    let transit_key = runtime
        .key_vault_transit_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "runtime.key_source=vault-transit requires runtime.key_vault_transit_key"
            )
        })?;
    let context = runtime
        .key_vault_transit_context
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "runtime.key_source=vault-transit requires runtime.key_vault_transit_context"
            )
        })?;
    let token = std::env::var(token_env)
        .map_err(|_| anyhow::anyhow!("environment variable {token_env} is not set"))?;
    let mount = runtime.key_vault_transit_mount.trim();
    let request_url = format!(
        "{}/v1/{}/hmac/{}/sha2-256",
        url.trim_end_matches('/'),
        mount,
        transit_key
    );
    let input = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(context.as_bytes())
    };
    let agent = build_vault_agent(runtime)?;
    let mut request = serde_json::json!({ "input": input });
    if let Some(version) = runtime.key_vault_transit_key_version {
        request["key_version"] = serde_json::json!(version);
    }
    let mut response = agent
        .post(&request_url)
        .header("X-Vault-Token", token.trim())
        .send_json(request)
        .map_err(|e| anyhow::anyhow!("failed to derive seal key from Vault transit: {e}"))?;
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| anyhow::anyhow!("failed to read Vault transit response body: {e}"))?;
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| anyhow::anyhow!("failed to parse Vault transit response JSON: {e}"))?;
    let hmac = json
        .get("data")
        .and_then(|value| value.get("hmac"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("Vault transit response did not contain data.hmac"))?;
    let derived = decode_vault_transit_hmac(hmac)?;
    tracing::info!(
        mount = mount,
        key = transit_key,
        key_version = runtime.key_vault_transit_key_version,
        "derived KEK seal key from Vault transit"
    );
    symm_key_from_exact_32(&derived, "Vault transit hmac")
}

async fn derive_kek_from_aws_kms_hmac_async(
    runtime: &common::config::RuntimeSection,
) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    let region = runtime
        .key_aws_kms_region
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("runtime.key_source=aws-kms-hmac requires runtime.key_aws_kms_region")
        })?;
    let key_id = runtime
        .key_aws_kms_key_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("runtime.key_source=aws-kms-hmac requires runtime.key_aws_kms_key_id")
        })?;
    let context = runtime
        .key_aws_kms_context
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("runtime.key_source=aws-kms-hmac requires runtime.key_aws_kms_context")
        })?;

    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_string()));
    if let Some(endpoint) = runtime.key_aws_kms_endpoint.as_deref() {
        let endpoint = endpoint.trim();
        if !endpoint.is_empty() {
            loader = loader.endpoint_url(endpoint.to_string());
        }
    }
    let config = loader.load().await;
    let client = aws_sdk_kms::Client::new(&config);
    let response = client
        .generate_mac()
        .key_id(key_id)
        .mac_algorithm(aws_sdk_kms::types::MacAlgorithmSpec::HmacSha256)
        .message(aws_sdk_kms::primitives::Blob::new(
            context.as_bytes().to_vec(),
        ))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("failed to derive seal key from AWS KMS GenerateMac: {e}"))?;
    let mac = response
        .mac
        .ok_or_else(|| anyhow::anyhow!("AWS KMS GenerateMac response did not contain mac bytes"))?;
    tracing::info!(
        region = region,
        key_id = key_id,
        "derived KEK seal key from AWS KMS HMAC"
    );
    symm_key_from_exact_32(mac.as_ref(), "AWS KMS GenerateMac")
}

fn load_kek_from_aws_kms_hmac(
    runtime: &common::config::RuntimeSection,
) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(derive_kek_from_aws_kms_hmac_async(runtime)))
    } else {
        let runtime_handle = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build tokio runtime for AWS KMS: {e}"))?;
        runtime_handle.block_on(derive_kek_from_aws_kms_hmac_async(runtime))
    }
}

fn seal_kek_blob(
    seal_key: &secrets::crypto::SymmetricKey,
    kek_bytes: &[u8],
) -> anyhow::Result<Vec<u8>> {
    Ok(secrets::crypto::encrypt(seal_key, kek_bytes)?.0)
}

const SECRET_TRANSPORT_KEY_META_KEY: &str = "secrets.transport_private_key";
const SEAL_KEY_DERIVATION_SALT_META_KEY: &str = "secrets.seal_key_derivation_salt";

fn load_or_create_seal_key_derivation_salt(store: &storage::Store) -> anyhow::Result<Vec<u8>> {
    if let Some(existing) = store.load_meta(SEAL_KEY_DERIVATION_SALT_META_KEY)? {
        return hex::decode(existing.trim())
            .map_err(|e| anyhow::anyhow!("failed to decode persisted seal-key salt: {e}"));
    }

    let salt = common::auth::AuthConfig::generate_token().into_bytes();
    store.save_meta(SEAL_KEY_DERIVATION_SALT_META_KEY, &hex::encode(&salt))?;
    tracing::info!("initialized seal-key derivation salt in redb");
    Ok(salt)
}

fn derive_seal_key_from_passphrase(
    passphrase: &str,
    salt: &[u8],
) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    let mut derived = [0u8; 32];
    argon2::Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut derived)
        .map_err(|e| anyhow::anyhow!("failed to derive seal key from passphrase: {e}"))?;
    Ok(secrets::crypto::SymmetricKey::from_bytes(derived))
}

fn resolve_persisted_seal_key(
    store: &storage::Store,
    runtime: &common::config::RuntimeSection,
) -> anyhow::Result<Option<secrets::crypto::SymmetricKey>> {
    match runtime.key_source.as_str() {
        "file" => {
            let key_file = runtime
                .key_file
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("runtime.key_source=file requires runtime.key_file"))?;

            let bytes = std::fs::read(key_file)
                .map_err(|e| anyhow::anyhow!("failed to read key file {}: {}", key_file, e))?;
            tracing::info!(path = %key_file, "loaded KEK seal key from key file");
            let seal_key = symm_key_from_exact_32(&bytes, &format!("key file {key_file}"))?;
            Ok(Some(seal_key))
        }
        "command" => Ok(Some(load_kek_from_command(&runtime.key_command)?)),
        "vault-kv" => Ok(Some(load_kek_from_vault_kv(runtime)?)),
        "vault-transit" => Ok(Some(load_kek_from_vault_transit(runtime)?)),
        "aws-kms-hmac" => Ok(Some(load_kek_from_aws_kms_hmac(runtime)?)),
        spec if spec.starts_with("env:") => Ok(Some(load_kek_from_env_spec(spec)?)),
        spec if spec.starts_with("passphrase-env:") => {
            let passphrase = load_passphrase_from_env_spec(spec)?;
            let salt = load_or_create_seal_key_derivation_salt(store)?;
            let seal_key = derive_seal_key_from_passphrase(&passphrase, &salt)?;
            tracing::info!("derived KEK seal key from operator-provided passphrase env source");
            Ok(Some(seal_key))
        }
        "generate" => Ok(None),
        other => anyhow::bail!(
            "unsupported runtime.key_source '{}'; supported values are 'generate', 'file', 'command', 'vault-kv', 'vault-transit', 'aws-kms-hmac', 'env:VAR_NAME', or 'passphrase-env:VAR_NAME'",
            other
        ),
    }
}

fn resolve_previous_persisted_seal_key(
    store: &storage::Store,
    runtime: &common::config::RuntimeSection,
) -> anyhow::Result<Option<secrets::crypto::SymmetricKey>> {
    match runtime.key_source.as_str() {
        "vault-transit" => {
            let Some(previous_version) = runtime.key_vault_transit_previous_key_version else {
                return Ok(None);
            };
            let mut previous = runtime.clone();
            previous.key_vault_transit_key_version = Some(previous_version);
            previous.key_vault_transit_previous_key_version = None;
            Ok(Some(load_kek_from_vault_transit(&previous)?))
        }
        "aws-kms-hmac" => {
            let Some(previous_key_id) = runtime.key_aws_kms_previous_key_id.as_ref() else {
                return Ok(None);
            };
            let mut previous = runtime.clone();
            previous.key_aws_kms_key_id = Some(previous_key_id.clone());
            previous.key_aws_kms_previous_key_id = None;
            Ok(Some(load_kek_from_aws_kms_hmac(&previous)?))
        }
        _ => {
            let _ = store;
            Ok(None)
        }
    }
}

fn load_or_create_persisted_kek(
    store: &storage::Store,
    seal_key: &secrets::crypto::SymmetricKey,
    previous_seal_key: Option<&secrets::crypto::SymmetricKey>,
) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    match store.load_kek()? {
        Some(bytes) if bytes.len() == 32 => {
            let legacy = symm_key_from_exact_32(&bytes, "legacy plaintext KEK")?;
            let sealed = seal_kek_blob(seal_key, legacy.as_bytes())?;
            store.save_kek(&sealed)?;
            tracing::warn!(
                "migrated legacy plaintext KEK in redb into a sealed-at-rest blob using the configured key source"
            );
            Ok(legacy)
        }
        Some(sealed_blob) => {
            let encrypted = secrets::crypto::EncryptedBlob(sealed_blob);
            let (plaintext, rewrap) = match secrets::crypto::decrypt(seal_key, &encrypted) {
                Ok(plaintext) => (plaintext, false),
                Err(current_error) => {
                    let previous = previous_seal_key.ok_or(current_error)?;
                    (secrets::crypto::decrypt(previous, &encrypted)?, true)
                }
            };
            if rewrap {
                store.save_kek(&seal_kek_blob(seal_key, &plaintext)?)?;
                tracing::warn!("rewrapped persisted KEK with the active external seal-key version");
            } else {
                tracing::info!("loaded sealed KEK from redb using configured key source");
            }
            symm_key_from_exact_32(&plaintext, "persisted sealed KEK")
        }
        None => {
            let initial_kek = symm_key_from_exact_32(
                seal_key.as_bytes(),
                "configured file/env key source initial KEK",
            )?;
            let sealed = seal_kek_blob(seal_key, initial_kek.as_bytes())?;
            store.save_kek(&sealed)?;
            tracing::info!("initialized sealed KEK in redb from configured key source");
            Ok(initial_kek)
        }
    }
}

fn load_or_create_persisted_secret_transport_keypair(
    store: &storage::Store,
    seal_key: &secrets::crypto::SymmetricKey,
    previous_seal_key: Option<&secrets::crypto::SymmetricKey>,
) -> anyhow::Result<secrets::BootstrapKeyPair> {
    match store.load_meta(SECRET_TRANSPORT_KEY_META_KEY)? {
        Some(sealed_hex) => {
            let sealed_blob = hex::decode(sealed_hex.trim()).map_err(|e| {
                anyhow::anyhow!("failed to decode sealed secret transport key from redb: {e}")
            })?;
            let encrypted = secrets::crypto::EncryptedBlob(sealed_blob);
            let (plaintext, rewrap) = match secrets::crypto::decrypt(seal_key, &encrypted) {
                Ok(plaintext) => (plaintext, false),
                Err(current_error) => {
                    let previous = previous_seal_key.ok_or(current_error)?;
                    (secrets::crypto::decrypt(previous, &encrypted)?, true)
                }
            };
            if plaintext.len() != 32 {
                anyhow::bail!(
                    "persisted sealed secret transport key must contain exactly 32 bytes, found {} bytes",
                    plaintext.len()
                );
            }
            let mut secret_bytes = [0u8; 32];
            secret_bytes.copy_from_slice(&plaintext);
            if rewrap {
                let resealed = secrets::crypto::encrypt(seal_key, &plaintext)?;
                store.save_meta(SECRET_TRANSPORT_KEY_META_KEY, &hex::encode(resealed.0))?;
                tracing::warn!(
                    "rewrapped node secret transport key with the active external seal-key version"
                );
            } else {
                tracing::info!("loaded sealed node secret transport key from redb");
            }
            Ok(secrets::BootstrapKeyPair::from_secret_bytes(secret_bytes))
        }
        None => {
            let keypair = secrets::BootstrapKeyPair::generate();
            let sealed_blob = secrets::crypto::encrypt(seal_key, &keypair.secret_bytes())?;
            store.save_meta(SECRET_TRANSPORT_KEY_META_KEY, &hex::encode(sealed_blob.0))?;
            tracing::info!("initialized sealed node secret transport key in redb");
            Ok(keypair)
        }
    }
}

fn is_loopback_host(host: &str) -> bool {
    let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
    trimmed.eq_ignore_ascii_case("localhost")
        || trimmed
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

fn normalize_artifact_base_url(raw: &str) -> anyhow::Result<String> {
    let url = Url::parse(raw.trim())
        .map_err(|e| anyhow::anyhow!("invalid advertised artifact URL '{}': {e}", raw.trim()))?;
    let mut normalized = url.to_string();
    while normalized.ends_with('/') {
        normalized.pop();
    }
    Ok(normalized)
}

fn host_for_socket_address(host: &str) -> String {
    let trimmed = host.trim();
    match trimmed.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) => format!("[{trimmed}]"),
        _ => trimmed.to_string(),
    }
}

const NODE_SUBSCRIPTION_SPECS: &[(&str, &str)] = &[
    ("DEPLOY", "deploy.>"),
    ("DEPLOY", "routes.>"),
    ("CONTROL", "instance.ready.>"),
    ("CONTROL", "instance.dead.>"),
    ("CONTROL", "secrets.update.>"),
    ("CONTROL", "secrets.delete.>"),
    ("CONTROL", "config.update.>"),
    ("CONTROL", "gateway.config.>"),
    ("NODE", "node.load.>"),
    ("NODE", "cluster.node_joined.>"),
    ("NODE", "cluster.snapshot.>"),
    ("HEALTH", "cluster.health.changed.>"),
    ("HEALTH", "cluster.health.snapshot.>"),
    ("PLATFORM", "platform.upgrade.>"),
    ("PLATFORM", "platform.upgrade_complete.>"),
    ("PLATFORM", "platform.draining.>"),
    ("PLATFORM", "config.hot_reload.>"),
    ("EBPF", "ebpf.pressure.*"),
    ("EBPF", "ebpf.pressure.recovered.*"),
    ("EBPF", "ebpf.security.incident.*"),
];

const NON_NODE_STREAM_SUBJECT_SPECS: &[(&str, &str)] = &[("PLATFORM", "platform.deploy_ingress.>")];

fn sanitize_subject(subject: &str) -> String {
    subject
        .replace('.', "-")
        .replace('>', "all")
        .replace('*', "one")
}

fn subject_matches_filter(subject: &str, filter: &str) -> bool {
    let subject_tokens: Vec<&str> = subject.split('.').collect();
    let filter_tokens: Vec<&str> = filter.split('.').collect();

    let mut subject_index = 0usize;
    for token in filter_tokens {
        match token {
            ">" => return true,
            "*" => {
                if subject_index >= subject_tokens.len() {
                    return false;
                }
                subject_index += 1;
            }
            literal => {
                if subject_tokens.get(subject_index) != Some(&literal) {
                    return false;
                }
                subject_index += 1;
            }
        }
    }

    subject_index == subject_tokens.len()
}

fn collect_missing_node_stream_subscriptions() -> Vec<(&'static str, &'static str)> {
    messaging::JETSTREAM_STREAM_SUBJECT_SPECS
        .iter()
        .filter(|(stream, _)| *stream != messaging::QUARANTINE_STREAM)
        .flat_map(|(stream, subjects)| {
            subjects
                .iter()
                .copied()
                .filter(move |subject| {
                    if NON_NODE_STREAM_SUBJECT_SPECS.iter().any(
                        |(excluded_stream, excluded_subject)| {
                            excluded_stream == stream && excluded_subject == subject
                        },
                    ) {
                        return false;
                    }
                    !NODE_SUBSCRIPTION_SPECS
                        .iter()
                        .any(|(subscription_stream, filter)| {
                            subscription_stream == stream && subject_matches_filter(subject, filter)
                        })
                })
                .map(move |subject| (*stream, subject))
        })
        .collect()
}

fn collect_unbacked_node_subscriptions() -> Vec<(&'static str, &'static str)> {
    NODE_SUBSCRIPTION_SPECS
        .iter()
        .copied()
        .filter(|(subscription_stream, filter)| {
            !messaging::JETSTREAM_STREAM_SUBJECT_SPECS
                .iter()
                .filter(|(stream, _)| *stream == *subscription_stream)
                .flat_map(|(_, subjects)| subjects.iter().copied())
                .any(|subject| subject_matches_filter(subject, filter))
        })
        .collect()
}

fn log_node_subscription_diagnostics() {
    let missing = collect_missing_node_stream_subscriptions();
    let unbacked = collect_unbacked_node_subscriptions();

    for (stream, subject) in &missing {
        warn!(
            stream = *stream,
            subject = *subject,
            "JetStream stream subject has no matching node durable subscription"
        );
    }

    for (stream, subject) in &unbacked {
        warn!(
            stream = *stream,
            subject = *subject,
            "node durable subscription is not backed by a declared JetStream stream subject"
        );
    }

    if missing.is_empty() && unbacked.is_empty() {
        info!("node subscription diagnostics: declared stream subjects are fully covered");
    }
}

fn bind_socket_address(host: &str, port: u16) -> anyhow::Result<String> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        anyhow::bail!("bind host must not be empty");
    }
    Ok(format!("{}:{}", host_for_socket_address(trimmed), port))
}

fn advertised_host_base_url(host: &str, port: u16) -> anyhow::Result<String> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        anyhow::bail!("admin.advertised_host must not be empty");
    }

    normalize_artifact_base_url(&format!(
        "http://{}:{}",
        host_for_socket_address(trimmed),
        port
    ))
}

fn build_artifact_server_url(admin: &common::config::AdminSection) -> anyhow::Result<String> {
    if let Some(url) = admin.advertised_artifact_url.as_deref() {
        return normalize_artifact_base_url(url);
    }
    if let Some(host) = admin.advertised_host.as_deref() {
        return advertised_host_base_url(host, admin.artifact_port);
    }
    Ok(format!("http://127.0.0.1:{}", admin.artifact_port))
}

fn build_proxy_advertised_address(config: &common::config::NodeConfig) -> anyhow::Result<String> {
    if let Some(host) = config.admin.advertised_host.as_deref() {
        return bind_socket_address(host, config.proxy.http_port);
    }
    Ok(format!("127.0.0.1:{}", config.proxy.http_port))
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn artifact_server_url_is_loopback(url: &str) -> bool {
    Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(ToOwned::to_owned))
        .map(|host| is_loopback_host(&host))
        .unwrap_or(false)
}

fn admin_tls_material(config: &common::config::NodeConfig) -> Option<(String, String)> {
    if let (Some(cert), Some(key)) = (config.admin.tls_cert.clone(), config.admin.tls_key.clone()) {
        return Some((cert, key));
    }
    if let (Some(cert), Some(key)) = (config.proxy.tls_cert.clone(), config.proxy.tls_key.clone()) {
        return Some((cert, key));
    }
    None
}

fn admin_tls_is_configured(config: &common::config::NodeConfig) -> bool {
    admin_tls_material(config).is_some()
}

async fn serve_admin_app(
    admin_addr: String,
    admin_app: axum::Router,
    tls_cert: Option<String>,
    tls_key: Option<String>,
) -> anyhow::Result<()> {
    if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
            .await
            .map_err(|e| anyhow::anyhow!("admin TLS config error: {e}"))?;
        let bind_addr: std::net::SocketAddr = admin_addr
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid admin bind address: {e}"))?;
        info!(addr = %admin_addr, "admin API listening with TLS");
        axum_server::bind_rustls(bind_addr, rustls_config)
            .serve(admin_app.into_make_service())
            .await
            .map_err(|e| anyhow::anyhow!("admin HTTPS server error: {e}"))?;
    } else {
        let listener = tokio::net::TcpListener::bind(&admin_addr)
            .await
            .map_err(|e| anyhow::anyhow!("admin API bind failed: {e}"))?;
        info!(addr = %admin_addr, "admin API listening");
        axum::serve(listener, admin_app.into_make_service())
            .await
            .map_err(|e| anyhow::anyhow!("admin HTTP server error: {e}"))?;
    }
    Ok(())
}

fn load_kek_from_config(
    store: &storage::Store,
    runtime: &common::config::RuntimeSection,
) -> anyhow::Result<secrets::crypto::SymmetricKey> {
    match resolve_persisted_seal_key(store, runtime)? {
        Some(seal_key) => {
            let previous = resolve_previous_persisted_seal_key(store, runtime)?;
            load_or_create_persisted_kek(store, &seal_key, previous.as_ref())
        }
        None => {
            if let Ok(Some(_persisted_kek)) = store.load_kek() {
                anyhow::bail!(
                    "persisted KEK detected in redb; key_source=generate cannot unlock or replace persisted secret state safely. Configure runtime.key_source=file, command, vault-kv, vault-transit, aws-kms-hmac, env:VAR_NAME, or passphrase-env:VAR_NAME to keep existing secrets"
                );
            }
            tracing::warn!(
                "key_source=generate: using ephemeral KEK; secrets created on this node will not survive restart"
            );
            Ok(secrets::crypto::SymmetricKey::generate())
        }
    }
}

fn load_secret_transport_keypair_from_config(
    store: &storage::Store,
    runtime: &common::config::RuntimeSection,
) -> anyhow::Result<secrets::BootstrapKeyPair> {
    match resolve_persisted_seal_key(store, runtime)? {
        Some(seal_key) => {
            let previous = resolve_previous_persisted_seal_key(store, runtime)?;
            load_or_create_persisted_secret_transport_keypair(store, &seal_key, previous.as_ref())
        }
        None => {
            if let Ok(Some(_persisted_transport_key)) =
                store.load_meta(SECRET_TRANSPORT_KEY_META_KEY)
            {
                anyhow::bail!(
                    "persisted secret transport key detected in redb; key_source=generate cannot unlock or replace transport identity safely. Configure runtime.key_source=file, command, vault-kv, vault-transit, aws-kms-hmac, env:VAR_NAME, or passphrase-env:VAR_NAME"
                );
            }
            tracing::warn!(
                "key_source=generate: using ephemeral node secret transport key; encrypted ctl-to-node secret rotation will require fresh cluster registry data after restart"
            );
            Ok(secrets::BootstrapKeyPair::generate())
        }
    }
}

use ebpf_monitor::{ActionDispatcher, EbpfMetrics, EventCallbacks, MonitorConfig};
use supervisor::SupervisorCommand;

mod dns_stub;

struct EbpfDependencyChecker {
    runtime: ebpf_monitor::MonitorRuntimeState,
}

impl EbpfDependencyChecker {
    fn new(runtime: ebpf_monitor::MonitorRuntimeState) -> Self {
        Self { runtime }
    }
}

impl proxy::health::DependencyChecker for EbpfDependencyChecker {
    fn name(&self) -> &str {
        "ebpf_monitoring"
    }

    fn check(&self) -> common::health::DependencyHealth {
        let availability = self.runtime.snapshot();
        let (status, message) = if !availability.enabled {
            (
                common::health::DependencyStatus::Healthy,
                "disabled by configuration".to_string(),
            )
        } else if !availability.monitoring_degraded {
            (
                common::health::DependencyStatus::Healthy,
                "kernel monitoring active".to_string(),
            )
        } else if availability.required {
            (
                common::health::DependencyStatus::Unhealthy,
                format!(
                    "required kernel monitoring unavailable: {}",
                    availability.reason.as_deref().unwrap_or("unknown")
                ),
            )
        } else {
            let mode = if availability.ebpf_active {
                "kernel monitoring incomplete"
            } else {
                "userspace fallback active"
            };
            (
                common::health::DependencyStatus::Degraded,
                format!(
                    "kernel monitoring degraded; {mode}: {}",
                    availability.reason.as_deref().unwrap_or("unknown")
                ),
            )
        };

        common::health::DependencyHealth {
            name: self.name().to_string(),
            status,
            message,
            latency_ms: None,
            last_check: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// Platform callbacks for the eBPF monitor's recovery actions.
///
/// This struct implements the `EventCallbacks` trait defined in the
/// `ebpf_monitor` crate, bridging kernel-level events to the platform's
/// actual components (backpressure signal, NATS health, event bus).
///
/// The eBPF monitor calls these methods when it detects anomalies that
/// require automated recovery actions. The decoupled design (trait object)
/// avoids circular dependencies between the `ebpf_monitor` and
/// `proxy`/`messaging` crates.
struct NodeEbpfCallbacks {
    backpressure: proxy::backpressure::BackpressureSignal,
    nats_health: Arc<NatsHealth>,
    bus: messaging::NatsBus,
    #[allow(dead_code)]
    node_id: String,
    /// Channel to send immediate action commands to the supervisor
    /// (kill largest instance, prune idle, remove from upstream).
    supervisor_tx: mpsc::Sender<SupervisorCommand>,
}

impl EventCallbacks for NodeEbpfCallbacks {
    fn activate_backpressure(&self, reason: &str) {
        warn!(reason, "eBPF: activating backpressure");
        self.backpressure.set_rejecting();
    }

    fn deactivate_backpressure(&self) {
        info!("eBPF: deactivating backpressure");
        self.backpressure.set_accepting();
    }

    fn mark_nats_disconnected(&self) {
        warn!("eBPF: pre-emptive NATS disconnect (TCP retransmits detected)");
        self.nats_health.mark_disconnected();
    }

    fn publish_node_under_pressure(&self, node_id: &str, pressure_level: u32) {
        let event = messaging::events::Event::NodeUnderPressure {
            node_id: node_id.to_string(),
            pressure_level,
        };
        let bus = self.bus.clone();
        tokio::spawn(async move {
            if let Err(e) = bus.publish(&event).await {
                tracing::warn!("Failed to publish pressure event: {}", e);
            }
        });
    }

    fn publish_node_pressure_recovered(&self, node_id: &str) {
        let event = messaging::events::Event::NodePressureRecovered {
            node_id: node_id.to_string(),
        };
        let bus = self.bus.clone();
        tokio::spawn(async move {
            if let Err(e) = bus.publish(&event).await {
                tracing::warn!("Failed to publish pressure recovered event: {}", e);
            }
        });
    }

    fn publish_security_incident(&self, node_id: &str, pid: u32, syscall_nr: u64, category: &str) {
        let event = messaging::events::Event::SecurityIncident {
            node_id: node_id.to_string(),
            app_id: String::new(), // Unknown at eBPF level
            pid,
            syscall_nr,
            category: category.to_string(),
        };
        let bus = self.bus.clone();
        tokio::spawn(async move {
            if let Err(e) = bus.publish(&event).await {
                tracing::warn!("Failed to publish security incident event: {}", e);
            }
        });
    }

    fn kill_instance(&self, pid: u32, reason: &str) {
        // Wasm instances run as in-process Tokio tasks, not separate OS
        // processes. The PID from eBPF refers to the node process itself
        // or a child process. We request the supervisor kill the largest
        // instance (most memory) as the best recovery action.
        warn!(
            pid,
            reason, "eBPF: kill instance requested - sending KillLargestInstance to supervisor"
        );
        if let Err(e) = self
            .supervisor_tx
            .try_send(SupervisorCommand::KillLargestInstance {
                reason: reason.to_string(),
            })
        {
            warn!(error = %e, "Failed to send KillLargestInstance command to supervisor");
        }
    }

    fn prune_idle_instances(&self) {
        // Kill all instances idle for more than 60 seconds to free FDs.
        warn!("eBPF: prune idle instances requested - sending PruneIdleInstances to supervisor");
        if let Err(e) = self
            .supervisor_tx
            .try_send(SupervisorCommand::PruneIdleInstances {
                idle_threshold_secs: 60,
            })
        {
            warn!(error = %e, "Failed to send PruneIdleInstances command to supervisor");
        }
    }

    fn remove_from_upstream(&self, pid: u32) {
        // The eBPF monitor detected a process exit. Since Wasm instances
        // are in-process, the health loop will handle cleanup. Log for
        // visibility - the PID may refer to a child process spawned by
        // a Wasm instance that has already exited.
        debug!(
            pid,
            "eBPF: remove from upstream requested - process exit detected, health loop will handle"
        );
    }

    fn kill_instance_by_tid(&self, tid: u32, reason: &str) {
        warn!(
            tid,
            reason, "eBPF: namespace security incident - kill instance by TID requested"
        );
        if let Err(e) = self
            .supervisor_tx
            .try_send(SupervisorCommand::KillInstanceByTid {
                tid,
                reason: reason.to_string(),
            })
        {
            warn!(
                error = %e,
                "Failed to send KillInstanceByTid command for security incident"
            );
        }
    }
}

mod args;
mod auth_reload;
pub mod db_config;
pub mod handlers;
pub mod log_reload;
pub mod recovery;
#[cfg(test)]
mod tests;
pub mod upgrade;

// Configuration is now handled by the `config` crate.
use args::Args;
use auth_reload::setup_sighup_handler;
use config::{load_config, CliOverrides};

fn clear_persisted_auth_override(db_path: &str) -> anyhow::Result<()> {
    if !std::path::Path::new(db_path).is_file() {
        anyhow::bail!(
            "refusing auth-override cleanup because redb file does not exist: {}",
            db_path
        );
    }
    let store = storage::Store::open(std::path::Path::new(db_path))?;
    store.delete_auth_config()?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // --generate-config: print a default TOML config to stdout and exit
    if args.generate_config {
        let default_config = common::config::NodeConfig::default();
        let toml_str =
            toml::to_string_pretty(&default_config).expect("failed to serialize default config");
        println!("{}", toml_str);
        return Ok(());
    }

    // --generate-tokens: generate random auth tokens and print to stdout, then exit
    if args.generate_tokens {
        let auth_config = common::auth::AuthConfig::generate_default();
        println!("# Add these to your config.toml under [auth] section:");
        println!("[auth]");
        println!("enabled = true");
        println!(
            "read_token = \"{}\"",
            auth_config.read_token.as_deref().unwrap_or("")
        );
        println!(
            "write_token = \"{}\"",
            auth_config.write_token.as_deref().unwrap_or("")
        );
        println!("require_tls = true");
        println!(
            "rate_limit_per_second = {}",
            auth_config.rate_limit_per_second
        );
        println!("rate_limit_burst = {}", auth_config.rate_limit_burst);
        return Ok(());
    }

    // --validate-config: load and validate a config file, then exit
    if let Some(ref path) = args.validate_config {
        match config::load_config(Some(std::path::Path::new(path)), &CliOverrides::default()) {
            Ok(_) => {
                println!("Configuration file '{}' is valid.", path);
                return Ok(());
            }
            Err(e) => {
                eprintln!("Configuration validation failed: {}", e);
                std::process::exit(1);
            }
        }
    }

    if args.clear_persisted_auth_override {
        clear_persisted_auth_override(&args.db_path)?;
        println!("Persisted auth override removed. Rotate external admin tokens before startup.");
        return Ok(());
    }

    // Convert CLI args to overrides for the config system
    let has_config_file = args.config.is_some();
    let cli_overrides = CliOverrides {
        node_id: (!has_config_file || args.node_id != "node-0").then(|| args.node_id.clone()),
        environment: None,
        db_path: (!has_config_file || args.db_path != "/tmp/wasm-node/state.redb")
            .then(|| args.db_path.clone()),
        nats_url: (!has_config_file || args.nats_url != "nats://127.0.0.1:4222")
            .then(|| args.nats_url.clone()),
        nats_creds: args.nats_creds.clone(),
        http_port: Some(args.proxy_port),
        https_port: Some(args.proxy_https_port),
        tls_cert: args.tls_cert.clone(),
        tls_key: args.tls_key.clone(),
        admin_port: Some(args.admin_port),
        artifact_port: Some(args.artifact_port),
        deploy_ingress_port: Some(args.deploy_ingress_port),
        admin_bind_address: args.admin_bind_address.clone(),
        artifact_bind_address: args.artifact_bind_address.clone(),
        deploy_ingress_bind_address: args.deploy_ingress_bind_address.clone(),
        admin_tls_cert: args.admin_tls_cert.clone(),
        admin_tls_key: args.admin_tls_key.clone(),
        admin_advertised_host: args.admin_advertised_host.clone(),
        admin_advertised_artifact_url: args.admin_advertised_artifact_url.clone(),
        port_start: Some(args.port_start),
        port_end: Some(args.port_end),
        key_source: (!has_config_file || args.key_source != "generate")
            .then(|| args.key_source.clone()),
        key_file: args.key_file.clone(),
        key_command: if args.key_command.is_empty() {
            None
        } else {
            Some(args.key_command.clone())
        },
        key_vault_url: args.key_vault_url.clone(),
        key_vault_token_env: args.key_vault_token_env.clone(),
        key_vault_ca_cert: args.key_vault_ca_cert.clone(),
        key_vault_mount: args.key_vault_mount.clone(),
        key_vault_path: args.key_vault_path.clone(),
        key_vault_field: args.key_vault_field.clone(),
        key_vault_transit_mount: args.key_vault_transit_mount.clone(),
        key_vault_transit_key: args.key_vault_transit_key.clone(),
        key_vault_transit_context: args.key_vault_transit_context.clone(),
        key_aws_kms_region: args.key_aws_kms_region.clone(),
        key_aws_kms_endpoint: args.key_aws_kms_endpoint.clone(),
        key_aws_kms_key_id: args.key_aws_kms_key_id.clone(),
        key_aws_kms_context: args.key_aws_kms_context.clone(),
        runtime_cache_directory: args.runtime_cache_directory.clone(),
        runtime_upgrade_signing_public_key: args.runtime_upgrade_signing_public_key.clone(),
        runtime_pooling_allocator: args.runtime_pooling_allocator,
        runtime_pooling_total_component_instances: args.runtime_pooling_total_component_instances,
        runtime_pooling_max_core_instances_per_component: args
            .runtime_pooling_max_core_instances_per_component,
        runtime_pooling_max_memories_per_component: args.runtime_pooling_max_memories_per_component,
        runtime_pooling_max_tables_per_component: args.runtime_pooling_max_tables_per_component,
        database_url: Some(args.database_url.clone()),
        pgbouncer_addr: Some(args.pgbouncer_addr.clone()),
        enable_db_proxy: Some(args.enable_db_proxy),
        db_proxy_addr: Some(args.db_proxy_addr.clone()),
        db_proxy_backend: Some(args.db_backend_addr.clone()),
        db_proxy_max_connections: Some(args.db_proxy_max_connections),
        log_level: Some(args.log_level.clone()),
        otlp_endpoint: args.otlp_endpoint.clone(),
        billing_export_dir: args.billing_export_dir.clone(),
        billing_export_interval_secs: Some(args.billing_export_interval_secs),
        platform_domain: args.platform_domain.clone(),
        dns_webhook_url: args.dns_webhook_url.clone(),
        dns_webhook_token: args.dns_webhook_token.clone(),
        auth_token: args.admin_token.clone(),
        auth_enabled: args.auth_enabled,
        auth_read_token: args.auth_read_token.clone(),
        auth_write_token: args.auth_write_token.clone(),
        auth_require_tls: args.auth_require_tls,
        auth_rate_limit_per_second: args.auth_rate_limit_per_second,
        auth_rate_limit_burst: args.auth_rate_limit_burst,
    };

    // Load configuration with merge priority: defaults < TOML < env < CLI
    let config_path = args.config.as_deref().map(std::path::Path::new);
    let config = load_config(config_path, &cli_overrides)?;

    // Set up structured logging with reload handle (allows runtime log-level changes)
    let format = match config.logging.format.as_str() {
        "text" => common::logging::LogFormat::Text,
        _ => common::logging::LogFormat::Json,
    };

    let output = if let Some(ref path) = args.log_output {
        common::logging::LogOutput::File {
            path: std::path::PathBuf::from(path),
        }
    } else if let Some(ref path) = config.logging.output {
        common::logging::LogOutput::File {
            path: std::path::PathBuf::from(path),
        }
    } else {
        common::logging::LogOutput::Stdout
    };

    let logging_config = common::logging::LoggingConfig {
        format,
        output,
        default_level: config.logging.level.clone(),
        module_levels: config.logging.modules.clone(),
        sampling_enabled: args.log_sampling || config.logging.sampling.enabled,
        info_sample_rate: if args.log_info_sample_rate != 1 {
            args.log_info_sample_rate
        } else {
            config.logging.sampling.info_rate
        },
        debug_sample_rate: if args.log_debug_sample_rate != 10 {
            args.log_debug_sample_rate
        } else {
            config.logging.sampling.debug_rate
        },
        trace_sample_rate: if args.log_trace_sample_rate != 100 {
            args.log_trace_sample_rate
        } else {
            config.logging.sampling.trace_rate
        },
        node_id: config.node.node_id.clone(),
        include_source: cfg!(debug_assertions),
    };

    let (log_reload_handle, _tracing_guard) =
        if let Some(endpoint) = config.logging.otlp_endpoint.as_deref() {
            let (reload, guard) = metrics::tracing_setup::init_tracing(
                "wasm-node",
                &config.node.node_id,
                endpoint,
                &logging_config,
            )
            .map_err(anyhow::Error::msg)?;
            (reload, Some(guard))
        } else {
            (common::logging::init_logging(&logging_config), None)
        };

    info!(node_id = %config.node.node_id, "wasm-node starting");
    info!(
        config_merge = "defaults + TOML + env + CLI",
        "configuration loaded"
    );

    if let Some(parent) = config.storage.db_path.parent() {
        std::fs::create_dir_all(parent).unwrap_or_default();
    }

    let store = match storage::Store::open(&config.storage.db_path) {
        Ok(s) => {
            info!(path = %config.storage.db_path.display(), "storage opened");
            s
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                path = %config.storage.db_path.display(),
                mode = ?config.storage.open_failure_mode,
                "failed to open redb"
            );

            if !config.storage.db_path.exists() {
                return Err(anyhow::anyhow!(
                    "failed to open redb at {}: {}",
                    config.storage.db_path.display(),
                    e
                ));
            }

            let quarantined_path = recovery::quarantine_db_file(&config.storage.db_path, "open_failure")
                .map_err(|quarantine_err| {
                    anyhow::anyhow!(
                        "failed to open redb at {}: {}. also failed to quarantine the unreadable DB: {}",
                        config.storage.db_path.display(),
                        e,
                        quarantine_err
                    )
                })?;

            match config.storage.open_failure_mode {
                common::config::StorageOpenFailureMode::QuarantineAndFail => {
                    return Err(anyhow::anyhow!(
                        "failed to open redb at {}: {}. unreadable database quarantined to {}. refusing automatic local state recreation by default; set storage.open_failure_mode = \"quarantine_and_recreate\" only if you intentionally want a fresh local DB bootstrap",
                        config.storage.db_path.display(),
                        e,
                        quarantined_path.display()
                    ));
                }
                common::config::StorageOpenFailureMode::QuarantineAndRecreate => {
                    tracing::warn!(
                        original_path = %config.storage.db_path.display(),
                        quarantined_path = %quarantined_path.display(),
                        "quarantined unreadable redb and recreating a fresh local database due to explicit recovery mode"
                    );
                    let s = storage::Store::open(&config.storage.db_path).map_err(|e2| {
                        anyhow::anyhow!(
                            "failed to open a fresh redb at {} after quarantining {}: {}",
                            config.storage.db_path.display(),
                            quarantined_path.display(),
                            e2
                        )
                    })?;
                    info!(
                        path = %config.storage.db_path.display(),
                        quarantined_path = %quarantined_path.display(),
                        "storage recreated after quarantining unreadable DB"
                    );
                    s
                }
            }
        }
    };

    // Initialize hot-reloadable configuration handle.
    // Loads any persisted overrides from redb so they survive restarts.
    let hot_config_handle =
        config::HotConfigHandle::new(&config, store.clone(), config.node.node_id.clone())?;
    info!("hot config handle initialized (persisted overrides applied if any)");

    // -- Watch channels for hot-reloadable component config ------------
    // These allow the config sync loop to push updated values to
    // long-running background tasks without restarting them.

    // GC config watch (interval, disk threshold, keep versions, etc.)
    let initial_gc_config = common::gc::GcConfig {
        artifact_keep_versions: config.gc.artifact_keep_versions,
        metrics_retain_days: config.gc.metrics_retain_days,
        undeploy_grace_secs: config.gc.undeploy_grace_secs,
        gc_interval_secs: config.gc.gc_interval_secs,
        disk_warning_threshold: config.gc.disk_warning_threshold,
    };
    let (gc_config_tx, gc_config_rx) = tokio::sync::watch::channel(initial_gc_config);

    // Health-check interval watch
    let (health_interval_tx, health_interval_rx) =
        tokio::sync::watch::channel(config.health.check_interval_secs);

    // Start the GC loop with the watch receiver (hot-reloadable interval)
    storage::gc::start_gc_loop(
        store.clone(),
        gc_config_rx,
        None, // GC metrics not yet wired
    );
    info!("GC loop started (interval hot-reloadable via config sync)");

    // Initialize recovery metrics early (needed for recovery mode detection)
    let recovery_metrics = Arc::new(metrics::recovery::RecoveryMetrics::new());

    // Detect recovery mode (L4: total loss detection)
    let recovery_mode = recovery::detect_recovery_mode(&store, &config.node.node_id);
    match recovery_mode {
        recovery::RecoveryMode::Normal => {
            info!("normal startup - existing state found");
        }
        recovery::RecoveryMode::FullRebuild => {
            info!("recovery mode: full rebuild required - will request state from cluster");
            recovery_metrics.set_recovery_mode(1);
        }
        recovery::RecoveryMode::CorruptionDetected => {
            tracing::warn!("recovery mode: corruption detected - will attempt partial rebuild");
            recovery_metrics.set_recovery_mode(2);
        }
    }

    let runtime = runtime::WasmRuntime::new_with_runtime_config(Some(&config.runtime))
        .expect("Failed to create WasmRuntime");
    info!("Wasm runtime initialized (Cranelift AOT)");

    let bind_addr: IpAddr = config
        .runtime
        .instance_bind_address
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid runtime.instance_bind_address: {e}"))?;
    let port_alloc = Arc::new(supervisor::port_alloc::PortAllocator::new(
        bind_addr,
        config.runtime.port_start,
        config.runtime.port_end,
    ));

    let upstream_registry = Arc::new(proxy::upstream::UpstreamRegistry::default());
    let service_registry = Arc::new(supervisor::network::LocalServiceRegistry::default());
    let host_router = Arc::new(proxy::router::HostRouter::default());

    let mut bus = match &config.nats.creds_file {
        Some(creds) => messaging::NatsBus::connect_secure(&config.nats.url, creds).await?,
        None => messaging::NatsBus::connect(&config.nats.url).await?,
    };
    bus.set_node_id(config.node.node_id.clone());
    info!("NATS connected");

    bus.setup_jetstream().await?;

    // Initialize NATS health tracking for L5 (partition) recovery
    let nats_health = Arc::new(NatsHealth::new());
    nats_health.mark_connected();

    // Actively probe NATS so health reflects server stalls as well as socket disconnect events.
    let _nats_watcher_handle = NatsHealthWatcher::new(
        (*nats_health).clone(),
        bus.client().clone(),
        Duration::from_secs(5),
    )
    .start();

    recovery::startup_integrity_check(&store, bus.client(), &config.storage).await;

    let (event_tx, event_rx) = mpsc::channel::<messaging::events::Event>(1000);
    // Wire the event receiver to a publisher task that forwards events to NATS
    {
        let bus_for_publisher = bus.clone();
        tokio::spawn(async move {
            messaging::publisher::run_publisher(bus_for_publisher, event_rx).await;
        });
    }

    // Initialize database manager
    let db_config = db_config::DatabaseConfig {
        default_database_url: config.database.default_url.clone(),
        health_check_addr: config.database.pgbouncer_addr.clone(),
        health_check_interval_secs: 30,
        enable_builtin_proxy: config.database.enable_db_proxy,
        builtin_proxy_addr: config.database.db_proxy_addr.clone(),
        builtin_proxy_backend: config.database.db_proxy_backend.clone(),
        builtin_proxy_max_connections: config.database.db_proxy_max_connections,
    };

    let db_manager = db_config::DatabaseManager::new(db_config.clone());
    db_manager.initialize().await?;

    let instance_bind_address = config.runtime.instance_bind_address.clone();
    let env_resolver = Arc::new(move |config: &common::types::AppConfig, _host_port: u16| {
        let mut vars = Vec::new();
        for (k, v) in &config.env_vars {
            vars.push((k.clone(), v.clone()));
        }
        if !config.env_vars.contains_key("BIND_ADDR") {
            vars.push(("BIND_ADDR".to_string(), instance_bind_address.clone()));
        }
        if !config.env_vars.contains_key("HOST") {
            vars.push(("HOST".to_string(), instance_bind_address.clone()));
        }
        // Inject DATABASE_URL if not already provided in env_vars
        if !config.env_vars.contains_key("DATABASE_URL") {
            vars.push((
                "DATABASE_URL".to_string(),
                db_config.default_database_url.clone(),
            ));
        }
        vars
    });

    // Initialize billing collector
    let billing_collector =
        billing::BillingCollector::start(store.clone(), config.node.node_id.clone());
    info!("billing collector started");

    // Optionally start billing export loop
    if let Some(ref export_dir) = config.billing.export_dir {
        let exporter = Arc::new(billing::FileExporter::new(std::path::PathBuf::from(
            export_dir,
        )));
        let interval = Duration::from_secs(config.billing.export_interval_secs);
        billing::start_export_loop(store.clone(), exporter, interval);
        info!(
            dir = export_dir,
            interval = interval.as_secs(),
            "billing export loop started"
        );
    }

    // eBPF monitor initialization is moved to after the backpressure signal
    // and supervisor are created, so the ActionDispatcher can reference them.
    // See below (after line ~535).

    let mut supervisor = supervisor::Supervisor::new(
        store.clone(),
        config.node.node_id.clone(),
        runtime.clone(),
        port_alloc.clone(),
        upstream_registry.clone(),
        host_router.clone(),
        service_registry.clone(),
        config.ebpf.gateway_port,
        env_resolver,
        event_tx.clone(),
        Some(billing_collector.tx()),
    );

    supervisor.restore_from_storage().await?;
    info!("supervisor state restored from storage");

    // Set the health interval watch before starting the loop.
    // set_health_interval_rx requires &mut Self, but Supervisor is behind Arc.
    // Arc::get_mut only succeeds if there's a single owner. Since supervisor
    // was just created by Supervisor::new (which returns Arc<Self>), we are
    // the sole owner at this point.
    if let Some(sup) = Arc::get_mut(&mut supervisor) {
        sup.set_health_interval_rx(health_interval_rx);
    } else {
        tracing::warn!("could not set health interval watch - supervisor already shared");
    }

    supervisor.clone().start_health_loop();
    supervisor.clone().start_command_loop();

    host_router.load_routes_from_store(&store).await;
    info!("routes loaded from local storage");

    // Initialize secret provider with KEK.
    //
    // Hardened key-source behavior:
    //   - `file`: load the raw 32-byte KEK from `runtime.key_file`
    //   - `command`: execute `runtime.key_command` and read a raw 32-byte or 64-hex-char seal key
    //   - `env:VAR_NAME`: load the KEK from an environment variable
    //   - `generate`: create an ephemeral KEK for this process only
    //
    // Plaintext KEK persistence in redb is no longer used for normal operation.
    // A legacy persisted KEK can be migrated into `runtime.key_file` when
    // `key_source=file` is configured and the file does not yet exist.
    let kek = load_kek_from_config(&store, &config.runtime)?;
    let secret_transport_keypair = Arc::new(load_secret_transport_keypair_from_config(
        &store,
        &config.runtime,
    )?);
    let artifact_transfer_authority = common::artifact_transfer::ArtifactTransferAuthority::derive(
        &config.node.node_id,
        kek.as_bytes(),
    );
    let secret_provider = Arc::new(secrets::LocalSecretProvider::new(store.clone(), kek));

    // Determine whether this node still needs bootstrap. An empty node that has
    // already completed a valid bootstrap session (including an empty snapshot)
    // should not re-bootstrap forever on restart.
    let bootstrap_completed = store
        .load_meta(handlers::BOOTSTRAP_APPLIED_META_KEY)
        .ok()
        .flatten()
        .is_some();
    let needs_bootstrap = store.list_apps()?.is_empty() && !bootstrap_completed;
    let bootstrap_session = if needs_bootstrap {
        let session_id = common::auth::AuthConfig::generate_token();
        let nonce = common::auth::AuthConfig::generate_token();
        let pending = serde_json::json!({
            "session_id": session_id,
            "nonce": nonce,
            "requested_at_ms": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        });
        store
            .save_meta(handlers::BOOTSTRAP_PENDING_META_KEY, &pending.to_string())
            .map_err(anyhow::Error::from)?;
        Some(Arc::new(tokio::sync::Mutex::new(
            handlers::BootstrapSessionState {
                session_id,
                nonce,
                keypair: secrets::BootstrapKeyPair::generate(),
                applied: false,
            },
        )))
    } else {
        None
    };

    let artifact_server_url = build_artifact_server_url(&config.admin)?;
    let proxy_address = build_proxy_advertised_address(&config)?;
    let node_load_table = Arc::new(proxy::node_table::NodeLoadTable::default());
    store.save_cluster_node(&common::types::ClusterNodeRecord {
        node_id: config.node.node_id.clone(),
        last_seen_unix_secs: now_unix_secs(),
        joined_at_unix_secs: Some(now_unix_secs()),
        health_status: common::health::NodeHealthStatus::Healthy,
        proxy_address: Some(proxy_address.clone()),
        artifact_server_url: Some(artifact_server_url.clone()),
        protocol_version: Some(common::protocol::PROTOCOL_VERSION),
        binary_version: Some(common::protocol::BINARY_VERSION.to_string()),
        secret_transport_public_key: Some(hex::encode(secret_transport_keypair.public_bytes())),
        accepting_requests: None,
        active_instances: Some(0),
        deployed_apps: Some(0),
    })?;
    if config.admin.advertised_artifact_url.is_some() || config.admin.advertised_host.is_some() {
        info!(artifact_server_url = %artifact_server_url, manifest_auth = true, "using configured advertised artifact endpoint");
    } else {
        info!(artifact_server_url = %artifact_server_url, "using local-only default advertised artifact endpoint");
    }

    // -- Gateway Setup (early, so EventDispatcher can reference it) ----
    let oidc_provider = config.gateway.oidc.as_ref().map(|oidc_cfg| {
        let provider = Arc::new(proxy::gateway::oidc::OidcProvider::new(oidc_cfg.clone()));
        provider.clone().start_refresh_loop();
        tracing::info!(issuer = %oidc_cfg.issuer_url, "OIDC provider initialized");
        provider
    });
    let gateway = Arc::new(proxy::gateway::Gateway::new(oidc_provider));

    // Construct the application limiter before the event dispatcher so
    // DeployApp and ConfigUpdate can apply per-application overrides.
    let initial_hot = hot_config_handle.read().await;
    let default_rate_config = proxy::rate_limiter::RateLimitConfig {
        requests_per_second: initial_hot.rate_limit.default_requests_per_second,
        burst_capacity: initial_hot.rate_limit.default_burst_capacity,
        per_ip_limit: initial_hot.rate_limit.default_per_ip_limit,
    };
    drop(initial_hot);
    let rate_limiter = Arc::new(proxy::rate_limiter::RateLimiter::new(default_rate_config));
    let backpressure = proxy::backpressure::BackpressureSignal::new();

    // -- Embedded DNS Stub (resolves *.internal without external DNS) --
    let _dns_stub_addr = if config.dns.stub_enabled {
        match dns_stub::start_dns_stub(
            format!("127.0.0.1:{}", config.dns.stub_port)
                .parse()
                .unwrap(),
        )
        .await
        {
            Ok(addr) => {
                info!(%addr, "embedded DNS stub started for *.internal resolution");
                Some(addr)
            }
            Err(e) => {
                warn!(error = %e, "failed to start embedded DNS stub");
                None
            }
        }
    } else {
        None
    };

    let dispatcher = Arc::new(handlers::EventDispatcher {
        supervisor: supervisor.clone(),
        upstream: upstream_registry.clone(),
        host_router: host_router.clone(),
        store: store.clone(),
        runtime: runtime.clone(),
        node_id: config.node.node_id.clone(),
        artifact_server_url: artifact_server_url.clone(),
        artifact_transfer_authority: artifact_transfer_authority.clone(),
        upgrade_signing_public_key: config.runtime.upgrade_signing_public_key.clone(),
        secret_provider: secret_provider.clone(),
        secret_transport_keypair: secret_transport_keypair.clone(),
        bootstrap_session: bootstrap_session.clone(),
        bus: bus.clone(),
        dns_webhook: proxy::dns_webhook::DnsWebhookManager::new(
            config.dns.webhook_url.clone(),
            config.dns.webhook_token.clone(),
        ),
        node_table: node_load_table.clone(),
        rate_limiter: rate_limiter.clone(),
        backpressure: backpressure.clone(),
        cluster_node_stale_after_secs: config.health.cluster_node_stale_after_secs,
        gateway: Some(gateway.clone()),
    });

    // Subscribe to control-plane streams with subject-filtered durable consumers.
    // This avoids duplicate delivery when a stream carries multiple event classes.
    log_node_subscription_diagnostics();
    for (stream, subject) in NODE_SUBSCRIPTION_SPECS {
        let d = dispatcher.clone();
        let consumer = format!("node-{}-{}", config.node.node_id, sanitize_subject(subject));
        tracing::info!(stream, subject, consumer = %consumer, "subscribing durable consumer");
        bus.subscribe_durable(stream, &consumer, Some(subject), move |event| {
            let d = d.clone();
            async move { d.handle(event).await }
        })
        .await?;
    }

    // If this is a fresh node, request state snapshot from cluster
    if needs_bootstrap {
        info!("fresh node detected - requesting state snapshot from cluster");
        if artifact_server_url_is_loopback(&artifact_server_url) {
            warn!(
                artifact_server_url = %artifact_server_url,
                "fresh node is advertising a loopback artifact endpoint; this only works for same-host/local-only setups. Configure admin.advertised_host or admin.advertised_artifact_url for routable multi-node exchange"
            );
        }

        let (bootstrap_session_id, bootstrap_nonce, public_key_bytes) = {
            let state = bootstrap_session
                .as_ref()
                .expect("bootstrap session should exist for fresh node")
                .lock()
                .await;
            (
                state.session_id.clone(),
                state.nonce.clone(),
                state.keypair.public_bytes(),
            )
        };

        let join_event = messaging::events::Event::NodeJoined {
            node_id: config.node.node_id.clone(),
            bootstrap_session_id,
            bootstrap_nonce,
            artifact_server_url: artifact_server_url.clone(),
            public_key_bytes,
            protocol_version: common::protocol::PROTOCOL_VERSION,
            binary_version: common::protocol::BINARY_VERSION.to_string(),
        };

        bus.publish(&join_event).await?;
        info!("NodeJoined event published, waiting for snapshot");

        // Wait for StateSnapshot with a timeout instead of fixed sleep
        let snapshot_subject = format!("cluster.snapshot.{}", config.node.node_id);
        let timeout = tokio::time::Duration::from_secs(30);
        match tokio::time::timeout(timeout, bus.wait_for_event(&snapshot_subject)).await {
            Ok(Ok(_)) => info!("State snapshot received"),
            Ok(Err(e)) => warn!(error = %e, "failed to receive state snapshot"),
            Err(_) => warn!("timed out waiting for state snapshot after 30s"),
        }
    }

    supervisor::scaling::start_load_reporter(
        supervisor.clone(),
        bus.clone(),
        config.node.node_id.clone(),
        proxy_address.clone(),
        5_000_000_000,
    );

    let cold_start_supervisor = supervisor.clone();
    let cold_start = Arc::new(move |app_id: common::types::AppId| {
        let sup = cold_start_supervisor.clone();
        Box::pin(async move { sup.ensure_instance(&app_id).await.ok() })
            as futures::future::BoxFuture<'static, Option<std::net::SocketAddr>>
    });

    // Initialize Prometheus metrics
    let prom_metrics = Arc::new(metrics::exporter::Metrics::new());
    supervisor.set_policy_metrics(Arc::new(prom_metrics.policy.clone()));
    prom_metrics.set_platform_info(
        &config.node.node_id,
        common::protocol::BINARY_VERSION,
        common::protocol::PROTOCOL_VERSION,
    );
    info!(
        node_id = %config.node.node_id,
        binary_version = common::protocol::BINARY_VERSION,
        protocol_version = common::protocol::PROTOCOL_VERSION,
        "platform version metrics initialized"
    );

    // Initialize health check metrics
    let health_metrics = Arc::new(metrics::health_metrics::HealthMetrics::new(
        &prom_metrics.registry,
    ));
    info!("health check metrics registered with Prometheus");

    // -- Initialize eBPF monitor (kernel-level observability) ------------
    // The eBPF monitor provides kernel-level monitoring for memory pressure,
    // FD exhaustion, TCP retransmits, disk I/O latency, and syscall anomalies.
    // It uses eBPF programs on Linux >= 5.8 with BTF, or falls back to
    // userspace polling on other platforms.
    let ebpf_config = MonitorConfig::from_ebpf_section(&config.ebpf);
    let ebpf_metrics = Arc::new(EbpfMetrics::new(&prom_metrics.registry));
    let ebpf_callbacks: Arc<dyn EventCallbacks> = Arc::new(NodeEbpfCallbacks {
        backpressure: backpressure.clone(),
        nats_health: nats_health.clone(),
        bus: bus.clone(),
        node_id: config.node.node_id.clone(),
        supervisor_tx: supervisor.command_tx(),
    });
    let ebpf_dispatcher = Arc::new(ActionDispatcher::with_config(
        ebpf_metrics.clone(),
        ebpf_callbacks,
        config.node.node_id.clone(),
        ebpf_config.clone(),
    ));
    let node_pid = std::process::id();
    // Clone metrics and dispatcher for admin API before moving into init()
    let ebpf_metrics_admin = ebpf_metrics.clone();
    let ebpf_dispatcher_admin = ebpf_dispatcher.clone();
    let ebpf_dispatcher_sync = ebpf_dispatcher.clone();
    let ebpf_handle =
        ebpf_monitor::init(ebpf_config, ebpf_metrics, ebpf_dispatcher, node_pid).await;
    let ebpf_runtime_state = ebpf_handle.runtime_state();
    let ebpf_runtime_state_admin = ebpf_runtime_state.clone();
    let ebpf_namespace_map_admin = ebpf_handle.namespace_map.clone();
    let ebpf_startup = ebpf_runtime_state.snapshot();
    if config.ebpf.required && (!ebpf_startup.ebpf_active || ebpf_startup.monitoring_degraded) {
        let reason = ebpf_startup.reason.unwrap_or_else(|| "unknown".to_string());
        anyhow::bail!("required eBPF monitoring failed to initialize: {reason}");
    }
    if ebpf_handle.is_ebpf_active() {
        info!("eBPF monitor initialized with kernel-level monitoring");
    } else {
        info!("eBPF monitor running in userspace fallback mode (5s polling interval)");
    }

    // Wire namespace_map from eBPF monitor to supervisor for TID registration.
    // The supervisor has already been cloned by startup tasks, so this setter
    // uses interior mutability rather than relying on Arc::get_mut().
    supervisor.set_namespace_map(ebpf_handle.namespace_map.clone());

    // Initialize rate limit metrics (register with the same registry)
    let rate_limit_metrics = Arc::new(proxy::metrics::RateLimitMetrics::new(
        &prom_metrics.registry,
    ));

    let rate_limiter_sync = rate_limiter.clone();

    // -- Gateway Config Load -------------------------------------------
    // Gateway was created early so EventDispatcher can reference it.
    // Load persisted configs and API keys into the in-memory cache.
    match store.list_gateway_configs() {
        Ok(configs) => {
            let count = configs.len();
            for (app_id, cfg) in configs {
                gateway.set_route_config(&app_id, cfg).await;
            }
            tracing::info!(count = count, "gateway configs loaded from storage");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load gateway configs from storage");
        }
    }

    // 3b. Load API keys from storage into gateway validators
    match store.list_apps() {
        Ok(app_ids) => {
            for app_id in app_ids {
                match store.load_api_keys(&app_id.0) {
                    Ok(keys) if !keys.is_empty() => {
                        let validator = proxy::gateway::api_key::ApiKeyValidator::new(keys);
                        gateway.set_api_key_validator(&app_id.0, validator).await;
                    }
                    _ => {}
                }
            }
            tracing::info!("api key validators loaded from storage");
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load app list for api keys");
        }
    }

    // 4. Setup NATS KV bucket for distributed rate limiting
    let rate_limit_kv = if config.gateway.rate_limit.kv_bucket.is_empty() {
        None
    } else {
        let js = async_nats::jetstream::new(bus.client().clone());
        match js
            .create_key_value(async_nats::jetstream::kv::Config {
                bucket: config.gateway.rate_limit.kv_bucket.clone(),
                max_age: std::time::Duration::from_secs(10),
                history: 1,
                ..Default::default()
            })
            .await
        {
            Ok(kv) => {
                tracing::info!(bucket = %config.gateway.rate_limit.kv_bucket, "NATS KV bucket created for rate limiting");
                Some(kv)
            }
            Err(e) => {
                tracing::warn!(error = %e, bucket = %config.gateway.rate_limit.kv_bucket, "failed to create NATS KV bucket");
                None
            }
        }
    };

    // 5. Create distributed rate limiters for apps with gateway rate limit configs
    if let Some(ref kv) = rate_limit_kv {
        let gateway_configs = store.list_gateway_configs().unwrap_or_default();
        for (app_id, cfg) in gateway_configs {
            if let Some(ref route_rl) = cfg.rate_limit {
                if route_rl.distributed {
                    let limiter = Arc::new(
                        proxy::gateway::distributed_limiter::DistributedRateLimiter::new(
                            app_id.clone(),
                            config.node.node_id.clone(),
                            proxy::gateway::distributed_limiter::DistributedRateLimitConfig {
                                global_rps: route_rl.requests_per_second,
                                per_node_burst: route_rl.burst_capacity,
                                sync_interval_ms: config.gateway.rate_limit.sync_interval_ms,
                                kv_bucket: config.gateway.rate_limit.kv_bucket.clone(),
                            },
                        ),
                    );
                    limiter.set_kv_store(kv.clone()).await;
                    let limiter_clone = limiter.clone();
                    limiter_clone.start_sync_loop();
                    gateway
                        .distributed_limiters
                        .write()
                        .await
                        .insert(app_id, limiter);
                }
            }
        }
    }

    // -- Internal Mesh Gateway -----------------------------------------
    // Starts a local Axum proxy for East-West traffic between apps.
    // Listens on a configured loopback port. Namespace isolation relies on
    // service discovery: the Supervisor only injects service URLs for
    // same-namespace apps. The gateway port is open to all namespaces.
    let internal_gw = internal_gateway::InternalGateway::new(
        service_registry.clone(),
        rate_limiter.clone(),
        gateway.circuit_breaker.clone(),
        gateway.clone(),
    )
    .with_bind_port(config.ebpf.gateway_port)
    .with_namespace_map(ebpf_handle.namespace_map.clone())
    .with_ebpf_active(ebpf_handle.is_ebpf_active())
    .with_cold_start(cold_start.clone());
    let ebpf_handle = Arc::new(tokio::sync::Mutex::new(ebpf_handle));
    tokio::spawn(async move {
        if let Err(e) = internal_gw.run().await {
            tracing::error!(error = %e, "internal gateway exited");
        }
    });
    info!(
        port = config.ebpf.gateway_port,
        "internal gateway started for East-West traffic"
    );

    let wasm_proxy = proxy::service::WasmProxy {
        router: host_router.clone(),
        upstream: upstream_registry.clone(),
        rate_limiter,
        node_table: node_load_table.clone(),
        local_node_id: config.node.node_id.clone(),
        cold_start,
        backpressure: backpressure.clone(),
        metrics: Some(rate_limit_metrics),
        gateway: gateway.clone(),
        max_body_size_bytes: 10 * 1024 * 1024, // 10 MB
    };

    let tls = match (&config.proxy.tls_cert, &config.proxy.tls_key) {
        (Some(cert), Some(key)) => Some(proxy::tls::tls_settings(
            std::path::Path::new(cert),
            std::path::Path::new(key),
        )),
        _ => None,
    };

    let proxy_timeouts = proxy::config::ProxyTimeouts::default();
    let proxy_server = proxy::ProxyServer::build(
        wasm_proxy,
        config.proxy.http_port,
        Some(config.proxy.https_port).filter(|&p| p > 0),
        tls,
        proxy_timeouts,
    );

    // Admin API with pgBouncer status endpoint and Prometheus metrics
    let pgbouncer_check_addr = config.database.pgbouncer_addr.clone();
    let db_path_clone = config.storage.db_path.clone();
    let store_gc = store.clone();
    let supervisor_gc = supervisor.clone();
    let supervisor_instances = supervisor.clone();
    let supervisor_kill = supervisor.clone();
    let store_billing = store.clone();
    let host_router_admin = host_router.clone();
    let ebpf_cmd_tx = supervisor.command_tx();

    // -- Health Check System -------------------------------------------
    let startup_complete = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let app_health_registry = Arc::new(tokio::sync::RwLock::new(
        proxy::health::AppHealthRegistry::new(),
    ));

    let health_state = proxy::health::HealthState {
        node_id: config.node.node_id.clone(),
        nats_health: nats_health.clone(),
        backpressure: Arc::new(backpressure.clone()),
        started_at: std::time::Instant::now(),
        startup_complete: startup_complete.clone(),
        instance_count_provider: supervisor.clone()
            as Arc<dyn proxy::health::InstanceCountProvider + Send + Sync>,
        dependency_checkers: Arc::new(vec![
            Box::new(proxy::health::NatsDependencyChecker::new(
                nats_health.clone(),
            )),
            Box::new(proxy::health::RedbDependencyChecker::new(store.clone())),
            Box::new(proxy::health::DiskDependencyChecker::new(
                config.storage.db_path.clone(),
                config.health.min_disk_free_bytes,
                config.health.min_disk_free_inodes,
            )),
            Box::new(proxy::health::MemoryDependencyChecker::new(
                config.health.max_memory_bytes,
            )),
            Box::new(EbpfDependencyChecker::new(ebpf_runtime_state.clone())),
        ]),
        app_health_registry: app_health_registry.clone(),
        config: proxy::health::HealthCheckConfig {
            min_disk_free_bytes: config.health.min_disk_free_bytes,
            min_disk_free_inodes: config.health.min_disk_free_inodes,
            max_memory_bytes: config.health.max_memory_bytes,
            failure_threshold: config.health.failure_threshold,
            success_threshold: config.health.success_threshold,
            check_interval: std::time::Duration::from_secs(config.health.check_interval_secs),
            check_timeout: std::time::Duration::from_secs(config.health.check_timeout_secs),
        },
    };

    // Wire app health registry into upstream registry
    {
        let upstream_inner = upstream_registry.app_health_registry.write().await;
        // The registry is already wired via clone; no action needed.
        // This block ensures the RwLock type matches.
        let _ = &*upstream_inner;
    }

    let health_router = proxy::health::health_router(health_state.clone());

    // Health event publisher and background loop
    let health_publisher = Arc::new(proxy::health_events::HealthEventPublisher::new(
        bus.clone(),
        config.node.node_id.clone(),
    ));
    let _health_loop_handle = proxy::health_events::start_health_loop(
        Arc::new(health_state.clone()),
        health_publisher.clone(),
    );

    // Start background per-app upstream health checker
    let _upstream_health_handle = {
        let upstream_checker =
            proxy::upstream_health::UpstreamHealthChecker::new(upstream_registry.clone());
        upstream_checker.start()
    };
    info!("upstream health checker started");

    // Spawn a task to periodically update health metrics from the health state
    {
        let hm = health_metrics.clone();
        let hs = Arc::new(health_state.clone());
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                interval.tick().await;

                // Evaluate dependencies for metrics update
                let mut dependencies = Vec::new();
                for checker in hs.dependency_checkers.iter() {
                    dependencies.push(checker.check());
                }
                dependencies.push(common::health::DependencyHealth {
                    name: "backpressure".to_string(),
                    status: if hs.backpressure.is_accepting() {
                        common::health::DependencyStatus::Healthy
                    } else {
                        common::health::DependencyStatus::Unhealthy
                    },
                    message: if hs.backpressure.is_accepting() {
                        "accepting requests".to_string()
                    } else {
                        "rejecting requests - node at capacity".to_string()
                    },
                    latency_ms: None,
                    last_check: chrono::Utc::now().to_rfc3339(),
                });

                let status = proxy::health::compute_status_for_probe(
                    &dependencies,
                    common::health::ProbeType::Readiness,
                );

                let report = common::health::NodeHealthReport {
                    status,
                    node_id: hs.node_id.clone(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    uptime_secs: hs.started_at.elapsed().as_secs(),
                    startup_complete: hs
                        .startup_complete
                        .load(std::sync::atomic::Ordering::Relaxed),
                    accepting_requests: hs.backpressure.is_accepting(),
                    active_instances: hs.instance_count_provider.active_instance_count(),
                    deployed_apps: hs.instance_count_provider.deployed_app_count(),
                    dependencies,
                    apps: hs.instance_count_provider.app_health_summaries(),
                };

                hm.update_from_report(&report);
            }
        });
    }

    // -- Admin API Authentication Setup ----------------------------------
    // Resolve the effective AuthConfig from: [auth] section > legacy admin.auth_token > defaults.
    // Persisted overrides from redb (token rotations) take the highest priority.
    let mut effective_auth_config: common::auth::AuthConfig = if config.auth.enabled {
        let ac: common::auth::AuthConfig = config.auth.clone().into();
        ac
    } else if config.admin.auth_token.is_some() {
        tracing::info!(
            "using legacy admin.auth_token as write token - \
             consider migrating to the [auth] section for separate read/write tokens"
        );
        common::auth::AuthConfig::from_legacy_token(config.admin.auth_token.as_deref().unwrap())
    } else {
        common::auth::AuthConfig::default()
    };

    let production_environment =
        config.node.environment == common::config::DeploymentEnvironment::Production;

    // Production credentials must remain under the external secret manager's
    // lifecycle. A legacy plaintext redb override is therefore a startup error.
    match store.load_auth_config() {
        Ok(Some(persisted)) => {
            if production_environment {
                anyhow::bail!(
                    "production startup rejected plaintext persisted auth override; remove it with the documented migration procedure and rotate the affected tokens in the external secret manager"
                );
            } else {
                tracing::info!(
                    "loaded persisted auth config from database (overrides TOML values)"
                );
                effective_auth_config = persisted;
            }
        }
        Ok(None) => {
            tracing::debug!("no persisted auth config found - using TOML/CLI values");
        }
        Err(e) => {
            if production_environment {
                return Err(anyhow::anyhow!(
                    "failed to verify absence of persisted auth override in production: {e}"
                ));
            }
            tracing::warn!(error = %e, "failed to load persisted auth config - falling back to TOML file values");
        }
    }

    // Validate the effective auth config
    if let Err(e) = effective_auth_config.validate() {
        anyhow::bail!("Invalid auth configuration: {}", e);
    }

    let admin_tls_enabled = admin_tls_is_configured(&config);
    proxy::auth_middleware::check_admin_tls_requirement(&effective_auth_config, admin_tls_enabled)
        .map_err(anyhow::Error::msg)?;

    // Check config file permissions (warn if world-readable)
    if let Some(ref config_path) = args.config {
        proxy::auth_middleware::check_config_file_permissions(std::path::Path::new(config_path));
    }

    // Create shared auth state for the middleware
    let auth_config_shared = Arc::new(tokio::sync::RwLock::new(effective_auth_config.clone()));
    let auth_metrics = Arc::new(proxy::auth_middleware::AuthMetrics::new(
        &prom_metrics.registry,
    ));
    let admin_rate_limiter = Arc::new(proxy::auth_middleware::AdminRateLimiter::new(
        effective_auth_config.rate_limit_per_second,
        effective_auth_config.rate_limit_burst,
    ));

    // Audit callback - bridges proxy auth middleware to supervisor audit trail

    let audit_fn: proxy::auth_middleware::AuditCallback = Arc::new(
        move |info: proxy::auth_middleware::AuditInfo| {
            let event_type = if info.status_code >= 400 {
                supervisor::audit::AuditEventType::AuthFailure
            } else {
                supervisor::audit::AuditEventType::AdminApiCall
            };
            let event = supervisor::audit::AuditEvent {
                timestamp: chrono::Utc::now().timestamp_millis() as u64,
                node_id: info.node_id.clone(),
                event_type,
                actor: format!("admin:{}", info.token_type),
                app_id: "_platform".to_string(),
                details: serde_json::json!({
                    "path": info.path,
                    "method": info.method,
                    "client_ip": info.client_ip.map(|ip| ip.to_string()).unwrap_or("unknown".to_string()),
                    "status_code": info.status_code,
                }),
            };
            supervisor::audit::write_audit_event("/var/log/wasm-node/audit.jsonl", &event);
        },
    );

    let auth_state = proxy::auth_middleware::AuthState {
        config: auth_config_shared.clone(),
        metrics: auth_metrics,
        rate_limiter: admin_rate_limiter.clone(),
        trusted_proxies: Arc::new(
            effective_auth_config
                .trusted_proxy_nets()
                .map_err(anyhow::Error::msg)?,
        ),
        audit_fn: Some(audit_fn),
        node_id: config.node.node_id.clone(),
    };

    if effective_auth_config.enabled {
        info!(
            "admin API authentication enabled (rate limit: {}/s per IP, burst: {})",
            effective_auth_config.rate_limit_per_second, effective_auth_config.rate_limit_burst,
        );
    } else {
        info!("admin API authentication disabled - all endpoints accessible without token");
    }

    // Clone for token rotation endpoint
    let rotate_auth_config = auth_config_shared.clone();
    let rotate_store = store.clone();
    let rotate_node_id = config.node.node_id.clone();
    let rotate_disabled = production_environment;

    let admin_app = axum::Router::new()
        .merge(health_router)
        .route(
            "/status/pgbouncer",
            axum::routing::get(move || {
                let addr = pgbouncer_check_addr.clone();
                async move {
                    let available = supervisor::db_proxy::check_pgbouncer(&addr).await;
                    let status = if available { "healthy" } else { "unavailable" };
                    axum::Json(serde_json::json!({
                        "status": status,
                        "address": addr,
                        "available": available,
                    }))
                }
            }),
        )
        .route(
            "/admin/instances/{app_id}",
            axum::routing::get({
                let supervisor = supervisor_instances.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>| {
                    let supervisor = supervisor.clone();
                    async move {
                        let app_id = common::types::AppId(app_id);
                        let instances = supervisor.list_instances(&app_id).await;
                        axum::Json(serde_json::json!({
                            "app_id": app_id.0,
                            "instances": instances.iter().map(|id| serde_json::json!({
                                "id": id.0.to_string(),
                            })).collect::<Vec<_>>(),
                            "count": instances.len(),
                        }))
                    }
                }
            }),
        )
        .route(
            "/admin/instances/{app_id}/kill",
            axum::routing::post({
                let supervisor = supervisor_kill.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>| {
                    let supervisor = supervisor.clone();
                    async move {
                        let app_id = common::types::AppId(app_id);
                        match supervisor.kill_all_instances(&app_id).await {
                            Ok(()) => (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({
                                    "status": "killed",
                                    "app_id": app_id.0,
                                    "message": "all instances killed"
                                })),
                            ),
                            Err(e) => (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "status": "error",
                                    "app_id": app_id.0,
                                    "message": format!("failed to kill instances: {e}")
                                })),
                            ),
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/billing/count",
            axum::routing::get({
                let store = store_billing.clone();
                move || {
                    let store = store.clone();
                    async move {
                        match store.get_all_billing_records() {
                            Ok(records) => axum::Json(serde_json::json!({
                                "count": records.len() as u64,
                            })),
                            Err(e) => axum::Json(serde_json::json!({
                                "count": 0,
                                "error": format!("{e}"),
                            })),
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/billing/verify",
            axum::routing::post({
                let store = store_billing.clone();
                move || {
                    let store = store.clone();
                    async move {
                        match store.get_all_billing_records() {
                            Ok(records) => match billing::verify_chain(&records) {
                                Ok(count) => (
                                    axum::http::StatusCode::OK,
                                    axum::Json(serde_json::json!({
                                        "valid": true,
                                        "count": count,
                                    })),
                                ),
                                Err(e) => (
                                    axum::http::StatusCode::OK,
                                    axum::Json(serde_json::json!({
                                        "valid": false,
                                        "error": format!("{:?}", e),
                                    })),
                                ),
                            },
                            Err(e) => (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "valid": false,
                                    "error": format!("failed to read records: {:?}", e),
                                })),
                            ),
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/rebuild",
            axum::routing::post(move || {
                let db_path = db_path_clone.clone();
                async move {
                    tracing::warn!("Admin rebuild requested - quarantining local state for rebuild");
                    match recovery::quarantine_db_file(&db_path, "admin_rebuild") {
                        Ok(quarantined_path) => (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "status": "rebuild_prepared",
                                "message": "Local state quarantined. Restart the node to rebuild from cluster state.",
                                "quarantined_path": quarantined_path.display().to_string()
                            })),
                        ),
                        Err(e) => {
                            tracing::error!(error = %e, "failed to quarantine database for rebuild");
                            (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "status": "error",
                                    "message": format!("failed to quarantine database: {e}")
                                })),
                            )
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/gc/force",
            axum::routing::post(move || {
                let store = store_gc.clone();
                let supervisor = supervisor_gc.clone();
                async move {
                    tracing::info!("Forcing immediate GC run");

                    // Force purge undeployed apps with grace period = 0
                    let purged = store.gc_undeployed_apps(0).unwrap_or(0);
                    tracing::info!(apps = purged, "Forced GC: undeployed apps purged");

                    // Only force-kill instances for undeployed apps (apps with no active routes).
                    // Killing instances for still-deployed apps would cause unnecessary disruption.
                    let app_ids = store.list_apps().unwrap_or_default();
                    let routes = store.list_routes().unwrap_or_default();
                    let routed_app_ids: Vec<String> =
                        routes.iter().map(|r| r.app_id.0.clone()).collect();
                    let mut killed_count = 0;

                    for app_id in &app_ids {
                        // Skip apps that still have active routes - they are still deployed
                        if routed_app_ids.contains(&app_id.0) {
                            continue;
                        }
                        let app_id_obj = common::types::AppId(app_id.0.clone());
                        match supervisor.kill_all_instances(&app_id_obj).await {
                            Ok(()) => {
                                killed_count += 1;
                            }
                            Err(e) => {
                                tracing::debug!(app = %app_id.0, error = %e, "No instances to kill");
                            }
                        }
                    }

                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "status": "gc_complete",
                            "undeployed_apps_purged": purged,
                            "apps_killed": killed_count,
                        })),
                    )
                }
            }),
        )
        .merge(metrics::exporter::metrics_router(prom_metrics))
        // -- eBPF Monitor Admin Endpoints ------------------------------
        .route(
            "/admin/ebpf/status",
            axum::routing::get(move || {
                let metrics = ebpf_metrics_admin.clone();
                let dispatcher = ebpf_dispatcher_admin.clone();
                let runtime = ebpf_runtime_state_admin.clone();
                async move {
                    let availability = runtime.snapshot();
                    let status = ebpf_monitor::MonitorStatus {
                        ebpf_active: availability.ebpf_active,
                        attached_programs: availability.attached_programs,
                        monitoring_required: availability.required,
                        monitoring_degraded: availability.monitoring_degraded,
                        monitoring_degraded_reason: availability.reason,
                        backpressure_active: dispatcher.is_backpressure_active(),
                        degraded_mode: dispatcher.is_degraded(),
                        pressure_level: dispatcher.last_pressure_level(),
                        oom_kills: metrics.oom_kills.get(),
                        process_exits: metrics.process_exits.get(),
                        tcp_retransmits: metrics.tcp_retransmits.get(),
                        security_violations: metrics.security_violations.get(),
                        events_processed: metrics.events_processed.get(),
                        events_parse_errors: metrics.events_parse_errors.get(),
                        fd_usage_ratio: metrics.get_fd_usage_ratio(),
                        memory_pressure_level: metrics.memory_pressure_level.get(),
                        tcp_connection_count: metrics.tcp_connection_count.get(),
                        fd_count: metrics.fd_count.get(),
                    };
                    axum::Json(status)
                }
            }),
        )
        .route(
            "/admin/ebpf/identities",
            axum::routing::get(move || {
                let namespace_map = ebpf_namespace_map_admin.clone();
                async move { axum::Json(namespace_map.status()) }
            }),
        )
        .route(
            "/admin/ebpf/config",
            axum::routing::post(move |body: axum::Json<serde_json::Value>| {
                let cmd_tx = ebpf_cmd_tx.clone();
                async move {
                    let body = body.0;
                    let mut actions = Vec::new();

                    // Action: prune idle instances to free FDs
                    if body.get("prune_idle").and_then(|v| v.as_bool()).unwrap_or(false) {
                        let threshold = body.get("idle_threshold_secs")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(60);
                        if let Err(e) = cmd_tx.try_send(SupervisorCommand::PruneIdleInstances {
                            idle_threshold_secs: threshold,
                        }) {
                            tracing::warn!(error = %e, "Failed to send PruneIdleInstances command");
                        } else {
                            actions.push("prune_idle");
                        }
                    }

                    // Action: kill the largest instance (most memory)
                    if body.get("kill_largest").and_then(|v| v.as_bool()).unwrap_or(false) {
                        let reason = body.get("kill_largest_reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("admin API request")
                            .to_string();
                        if let Err(e) = cmd_tx.try_send(SupervisorCommand::KillLargestInstance {
                            reason,
                        }) {
                            tracing::warn!(error = %e, "Failed to send KillLargestInstance command");
                        } else {
                            actions.push("kill_largest");
                        }
                    }

                    // Threshold updates (logged for future propagation to eBPF programs)
                    if let Some(thresholds) = body.get("thresholds") {
                        tracing::info!(
                            thresholds = ?thresholds,
                            "eBPF threshold update requested (propagation to eBPF programs pending)"
                        );
                        actions.push("threshold_update_logged");
                    }

                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "status": "ok",
                            "actions": actions,
                        })),
                    )
                }
            }),
        )
        .route(
            "/admin/routes",
            axum::routing::get({
                let router = host_router_admin.clone();
                move || {
                    let router = router.clone();
                    async move {
                        let routes = router.list_routes().await;
                        axum::Json(serde_json::json!({
                            "routes": routes,
                            "count": routes.len(),
                        }))
                    }
                }
            }),
        )
        // -- Gateway Configuration Endpoints -----------------------------
        .route(
            "/admin/gateway",
            axum::routing::get({
                let store = store.clone();
                move || {
                    let store = store.clone();
                    async move {
                        match store.list_gateway_configs() {
                            Ok(configs) => axum::Json(serde_json::json!({
                                "configs": configs.iter().map(|(app_id, cfg)| serde_json::json!({
                                    "app_id": app_id,
                                    "config": cfg,
                                })).collect::<Vec<_>>(),
                                "count": configs.len(),
                            })),
                            Err(e) => axum::Json(serde_json::json!({
                                "configs": Vec::<serde_json::Value>::new(),
                                "count": 0,
                                "error": format!("{e}"),
                            })),
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/gateway/{app_id}",
            axum::routing::get({
                let store = store.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>| {
                    let store = store.clone();
                    async move {
                        match store.load_gateway_config(&app_id) {
                            Ok(Some(config)) => (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({
                                    "app_id": app_id,
                                    "config": config,
                                })),
                            ),
                            Ok(None) => (
                                axum::http::StatusCode::NOT_FOUND,
                                axum::Json(serde_json::json!({
                                    "error": "not_found",
                                    "message": format!("no gateway config for {app_id}"),
                                })),
                            ),
                            Err(e) => (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "error": "storage_error",
                                    "message": format!("{e}"),
                                })),
                            ),
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/gateway/{app_id}",
            axum::routing::post({
                let store = store.clone();
                let bus = bus.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>,
                      axum::Json(body): axum::Json<common::types::GatewayRouteConfig>| {
                    let store = store.clone();
                    let bus = bus.clone();
                    async move {
                        let app_id = match common::types::AppId::new_validate(&app_id) {
                            Ok(id) => id,
                            Err(e) => {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "error": "invalid_app_id",
                                        "message": e,
                                    })),
                                );
                            }
                        };
                        if let Err(e) = store.save_gateway_config(&app_id.0, &body) {
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "error": "storage_error",
                                    "message": format!("{e}"),
                                })),
                            );
                        }
                        let event = messaging::events::Event::GatewayConfigUpdate {
                            app_id: app_id.clone(),
                            config: body,
                        };
                        if let Err(e) = bus.publish(&event).await {
                            tracing::warn!(error = %e, "failed to publish gateway config update");
                        }
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "status": "updated",
                                "app_id": app_id.0,
                            })),
                        )
                    }
                }
            }),
        )
        .route(
            "/admin/gateway/{app_id}",
            axum::routing::delete({
                let store = store.clone();
                let bus = bus.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>| {
                    let store = store.clone();
                    let bus = bus.clone();
                    async move {
                        let app_id = match common::types::AppId::new_validate(&app_id) {
                            Ok(id) => id,
                            Err(e) => {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "error": "invalid_app_id",
                                        "message": e,
                                    })),
                                );
                            }
                        };
                        if let Err(e) = store.delete_gateway_config(&app_id.0) {
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "error": "storage_error",
                                    "message": format!("{e}"),
                                })),
                            );
                        }
                        let event = messaging::events::Event::GatewayConfigRemove {
                            app_id: app_id.clone(),
                        };
                        if let Err(e) = bus.publish(&event).await {
                            tracing::warn!(error = %e, "failed to publish gateway config remove");
                        }
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "status": "removed",
                                "app_id": app_id.0,
                            })),
                        )
                    }
                }
            }),
        )
        .route(
            "/admin/cross-namespace-allowlist",
            axum::routing::get({
                let gateway = gateway.clone();
                move || {
                    let gateway = gateway.clone();
                    async move {
                        let rules = gateway.list_cross_namespace_rules().await;
                        axum::Json(serde_json::json!({
                            "rules": rules.iter().map(|(s, t)| serde_json::json!({"source": s, "target": t})).collect::<Vec<_>>(),
                            "count": rules.len(),
                        }))
                    }
                }
            }),
        )
        .route(
            "/admin/cross-namespace-allowlist",
            axum::routing::post({
                let gateway = gateway.clone();
                move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let gateway = gateway.clone();
                    async move {
                        let source = body.get("source").and_then(|v| v.as_str()).unwrap_or("");
                        let target = body.get("target").and_then(|v| v.as_str()).unwrap_or("");
                        if source.is_empty() || target.is_empty() {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({"error": "source and target required"})),
                            );
                        }
                        gateway.add_cross_namespace_rule(source, target).await;
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({"status": "added"})),
                        )
                    }
                }
            }),
        )
        .route(
            "/admin/cross-namespace-allowlist/{source}/{target}",
            axum::routing::delete({
                let gateway = gateway.clone();
                move |
                    axum::extract::Path((source, target)): axum::extract::Path<(String, String)>| {
                    let gateway = gateway.clone();
                    async move {
                        gateway.remove_cross_namespace_rule(&source, &target).await;
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({"status": "removed"})),
                        )
                    }
                }
            }),
        )
        .route(
            "/admin/cluster/nodes",
            axum::routing::get({
                let store = store.clone();
                let cluster_node_stale_after_secs = config.health.cluster_node_stale_after_secs;
                move || {
                    let store = store.clone();
                    async move {
                        match store.list_cluster_nodes() {
                            Ok(nodes) => (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({
                                    "nodes": nodes,
                                    "active_staleness_secs": cluster_node_stale_after_secs,
                                })),
                            ),
                            Err(e) => (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "error": "storage_error",
                                    "message": e.to_string(),
                                })),
                            ),
                        }
                    }
                }
            }),
        )
        // -- App Management Endpoints ------------------------------------
        .route(
            "/admin/apps",
            axum::routing::get({
                let store = store.clone();
                let supervisor = supervisor_instances.clone();
                move |axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>| {
                    let store = store.clone();
                    let supervisor = supervisor.clone();
                    async move {
                        let namespace = params.get("namespace").cloned().unwrap_or_else(|| "default".to_string());
                        match store.list_deployed_apps() {
                            Ok(app_ids) => {
                                let mut apps = Vec::new();
                                for app_id in app_ids {
                                    if app_id.namespace() == namespace {
                                        let instances = supervisor.list_instances(&app_id).await.len() as u64;
                                        apps.push(serde_json::json!({
                                            "id": app_id.0,
                                            "namespace": app_id.namespace(),
                                            "instances": instances,
                                        }));
                                    }
                                }
                                axum::Json(serde_json::json!(apps))
                            }
                            Err(e) => axum::Json(serde_json::json!({
                                "error": format!("{e}"),
                            })),
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/apps/{app_id}/manifest",
            axum::routing::get({
                let store = store.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>| {
                    let store = store.clone();
                    async move {
                        let app_id = match common::types::AppId::new_validate(&app_id) {
                            Ok(id) => id,
                            Err(e) => {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "error": "invalid_app_id",
                                        "message": e,
                                    })),
                                );
                            }
                        };
                        let config = match store.load_config(&app_id) {
                            Ok(Some(c)) => c,
                            Ok(None) => {
                                return (
                                    axum::http::StatusCode::NOT_FOUND,
                                    axum::Json(serde_json::json!({
                                        "error": "not_found",
                                        "message": format!("no config for {}", app_id.0),
                                    })),
                                );
                            }
                            Err(e) => {
                                return (
                                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                    axum::Json(serde_json::json!({
                                        "error": "storage_error",
                                        "message": format!("{e}"),
                                    })),
                                );
                            }
                        };
                        let gateway_config = store.load_gateway_config(&app_id.0).unwrap_or(None);
                        let api_keys = store.load_api_keys(&app_id.0).unwrap_or_default();
                        let manifest = serde_json::json!({
                            "app": {
                                "name": config.id.bare_app_name(),
                                "version": config.id.bare_name().split(':').nth(1).unwrap_or("v1"),
                                "namespace": config.namespace,
                                "wasm_bind_port": config.wasm_bind_port,
                            },
                            "fuel": {
                                "quota": config.fuel_quota.0,
                                "memory_pages": config.memory_limit.0,
                                "max_instances": config.max_instances,
                                "idle_timeout_secs": config.idle_timeout_secs,
                            },
                            "policy": config.policy,
                            "gateway": gateway_config,
                            "env": config.env_vars,
                            "secrets": config.secret_keys,
                            "api_keys": api_keys,
                        });
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(manifest),
                        )
                    }
                }
            }),
        )
        .route(
            "/admin/deploy/ingest",
            axum::routing::post({
                let store = store.clone();
                let secret_provider = secret_provider.clone();
                let artifact_server_url = artifact_server_url.clone();
                let artifact_transfer_authority = artifact_transfer_authority.clone();
                let node_id = config.node.node_id.clone();
                let cluster_node_stale_after_secs = config.health.cluster_node_stale_after_secs;
                move |axum::Json(body): axum::Json<common::deploy::RemoteArtifactIngressRequest>| {
                    let store = store.clone();
                    let secret_provider = secret_provider.clone();
                    let artifact_server_url = artifact_server_url.clone();
                    let artifact_transfer_authority = artifact_transfer_authority.clone();
                    let node_id = node_id.clone();
                    let cluster_node_stale_after_secs = cluster_node_stale_after_secs;
                    async move {
                        match handlers::ingest_remote_artifact(
                            &store,
                            secret_provider.as_ref(),
                            &artifact_server_url,
                            &artifact_transfer_authority,
                            &node_id,
                            cluster_node_stale_after_secs,
                            body.artifact,
                        )
                        .await
                        {
                            Ok(response) => (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::to_value(response).unwrap_or_else(|e| {
                                    serde_json::json!({
                                        "error": "serialization_error",
                                        "message": format!("{e}"),
                                    })
                                })),
                            ),
                            Err(err) => {
                                let status = match &err {
                                    common::error::PlatformError::ConfigValidation(_) => {
                                        axum::http::StatusCode::BAD_REQUEST
                                    }
                                    common::error::PlatformError::Security(_) => {
                                        axum::http::StatusCode::BAD_REQUEST
                                    }
                                    common::error::PlatformError::External { .. } => {
                                        axum::http::StatusCode::BAD_GATEWAY
                                    }
                                    common::error::PlatformError::Storage { .. }
                                    | common::error::PlatformError::Internal(_)
                                    | common::error::PlatformError::Io { .. } => {
                                        axum::http::StatusCode::INTERNAL_SERVER_ERROR
                                    }
                                    _ => axum::http::StatusCode::BAD_REQUEST,
                                };
                                (
                                    status,
                                    axum::Json(serde_json::json!({
                                        "error": "artifact_ingest_failed",
                                        "message": format!("{err}"),
                                    })),
                                )
                            }
                        }
                    }
                }
            })
            .layer(axum::extract::DefaultBodyLimit::max(32 * 1024)),
        )
        .route(
            "/admin/deploy/intent",
            axum::routing::post({
                let store = store.clone();
                let secret_provider = secret_provider.clone();
                let artifact_server_url = artifact_server_url.clone();
                let artifact_transfer_authority = artifact_transfer_authority.clone();
                let node_id = config.node.node_id.clone();
                let cluster_node_stale_after_secs = config.health.cluster_node_stale_after_secs;
                let bus = bus.clone();
                move |axum::Json(body): axum::Json<common::deploy::DeployIntentRequest>| {
                    let store = store.clone();
                    let secret_provider = secret_provider.clone();
                    let artifact_server_url = artifact_server_url.clone();
                    let artifact_transfer_authority = artifact_transfer_authority.clone();
                    let node_id = node_id.clone();
                    let bus = bus.clone();
                    let cluster_node_stale_after_secs = cluster_node_stale_after_secs;
                    async move {
                        match handlers::process_deploy_intent(
                            handlers::DeployIntentContext {
                                store: &store,
                                secret_provider: secret_provider.as_ref(),
                                artifact_server_url: &artifact_server_url,
                                artifact_transfer_authority: &artifact_transfer_authority,
                                node_id: &node_id,
                                cluster_node_stale_after_secs,
                                bus: &bus,
                            },
                            body,
                        )
                        .await
                        {
                            Ok(response) => (
                                axum::http::StatusCode::ACCEPTED,
                                axum::Json(serde_json::to_value(response).unwrap_or_else(|e| {
                                    serde_json::json!({
                                        "error": "serialization_error",
                                        "message": format!("{e}"),
                                    })
                                })),
                            ),
                            Err(err) => {
                                let status = match &err {
                                    common::error::PlatformError::ConfigValidation(_) => {
                                        axum::http::StatusCode::BAD_REQUEST
                                    }
                                    common::error::PlatformError::Security(_) => {
                                        axum::http::StatusCode::FORBIDDEN
                                    }
                                    common::error::PlatformError::External { .. } => {
                                        axum::http::StatusCode::BAD_GATEWAY
                                    }
                                    common::error::PlatformError::Storage { .. }
                                    | common::error::PlatformError::Internal(_)
                                    | common::error::PlatformError::Io { .. } => {
                                        axum::http::StatusCode::INTERNAL_SERVER_ERROR
                                    }
                                    _ => axum::http::StatusCode::BAD_REQUEST,
                                };
                                (
                                    status,
                                    axum::Json(serde_json::json!({
                                        "error": "deploy_intent_failed",
                                        "message": format!("{err}"),
                                    })),
                                )
                            }
                        }
                    }
                }
            })
            .layer(axum::extract::DefaultBodyLimit::max(256 * 1024)),
        )
        .route(
            "/admin/api_keys/{app_id}",
            axum::routing::post({
                let store = store.clone();
                let bus = bus.clone();
                move |axum::extract::Path(app_id): axum::extract::Path<String>,
                      axum::Json(body): axum::Json<Vec<common::types::ApiKeyRecord>>| {
                    let store = store.clone();
                    let bus = bus.clone();
                    async move {
                        let app_id = match common::types::AppId::new_validate(&app_id) {
                            Ok(id) => id,
                            Err(e) => {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "error": "invalid_app_id",
                                        "message": e,
                                    })),
                                );
                            }
                        };
                        if let Err(e) = store.save_api_keys(&app_id.0, &body) {
                            return (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "error": "storage_error",
                                    "message": format!("{e}"),
                                })),
                            );
                        }
                        // Publish event so all nodes update their validators
                        let event = messaging::events::Event::GatewayConfigUpdate {
                            app_id: app_id.clone(),
                            config: common::types::GatewayRouteConfig::default(),
                        };
                        let _ = bus.publish(&event).await;
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "status": "updated",
                                "app_id": app_id.0,
                                "key_count": body.len(),
                            })),
                        )
                    }
                }
            }),
        )
        // -- Configuration Management Endpoints --------------------------
        .route(
            "/admin/config",
            axum::routing::get({
                let cold = config.clone();
                let hot = hot_config_handle.clone();
                move || {
                    let cold = cold.clone();
                    let hot = hot.clone();
                    async move {
                        let hot_cfg = hot.read().await;
                        axum::Json(serde_json::json!({
                            "cold": {
                                "node_id": cold.node.node_id,
                                "nats_url": cold.nats.url,
                                "proxy_http_port": cold.proxy.http_port,
                                "proxy_https_port": cold.proxy.https_port,
                                "admin_port": cold.admin.port,
                                "artifact_port": cold.admin.artifact_port,
                                "db_path": cold.storage.db_path.to_string_lossy(),
                                "port_range": format!("{}-{}", cold.runtime.port_start, cold.runtime.port_end),
                                "database_url": cold.database.default_url,
                                "key_source": cold.runtime.key_source,
                            },
                            "hot": {
                                "rate_limit": hot_cfg.rate_limit,
                                "ebpf": hot_cfg.ebpf,
                                "gc": hot_cfg.gc,
                                "health": hot_cfg.health,
                                "logging": hot_cfg.logging,
                            },
                            "hot_reloadable_fields": [
                                "rate_limit.default_requests_per_second",
                                "rate_limit.default_burst_capacity",
                                "rate_limit.default_per_ip_limit",
                                "ebpf.fd_soft_limit",
                                "ebpf.fd_hard_limit",
                                "ebpf.mem_low_threshold_pages",
                                "ebpf.mem_critical_threshold_pages",
                                "ebpf.disk_slow_threshold_ns",
                                "ebpf.tcp_conn_limit_per_pid",
                                "ebpf.syscall_rate_limit",
                                "gc.gc_interval_secs",
                                "gc.disk_warning_threshold",
                                "health.check_interval_secs",
                                "health.default_idle_timeout_secs",
                                "logging.level",
                            ],
                        }))
                    }
                }
            }),
        )
        .route(
            "/admin/config",
            axum::routing::patch({
                let hot = hot_config_handle.clone();
                let log_h = log_reload_handle.clone();
                let nbus = bus.clone();
                let nid = config.node.node_id.clone();
                move |body: axum::Json<serde_json::Value>| {
                    let hot = hot.clone();
                    let log_h = log_h.clone();
                    let nbus = nbus.clone();
                    let nid = nid.clone();
                    async move {
                        let raw = body.0;
                        // Build a HotConfigUpdate from the JSON body
                        let update = config::HotConfigUpdate {
                            rate_limit_default_rps: raw.get("rate_limit_default_rps")
                                .and_then(|v| v.as_u64()).map(|v| v as u32),
                            rate_limit_default_burst: raw.get("rate_limit_default_burst")
                                .and_then(|v| v.as_u64()).map(|v| v as u32),
                            rate_limit_default_per_ip: raw.get("rate_limit_default_per_ip")
                                .and_then(|v| v.as_u64()).map(|v| v as u32),
                            ebpf_fd_soft_limit: raw.get("ebpf_fd_soft_limit")
                                .and_then(|v| v.as_u64()).map(|v| v as u32),
                            ebpf_fd_hard_limit: raw.get("ebpf_fd_hard_limit")
                                .and_then(|v| v.as_u64()).map(|v| v as u32),
                            ebpf_mem_low_threshold_pages: raw.get("ebpf_mem_low_threshold_pages")
                                .and_then(|v| v.as_u64()),
                            ebpf_mem_critical_threshold_pages: raw.get("ebpf_mem_critical_threshold_pages")
                                .and_then(|v| v.as_u64()),
                            ebpf_disk_slow_threshold_ns: raw.get("ebpf_disk_slow_threshold_ns")
                                .and_then(|v| v.as_u64()),
                            ebpf_tcp_conn_limit_per_pid: raw.get("ebpf_tcp_conn_limit_per_pid")
                                .and_then(|v| v.as_u64()).map(|v| v as u32),
                            ebpf_syscall_rate_limit: raw.get("ebpf_syscall_rate_limit")
                                .and_then(|v| v.as_u64()),
                            gc_interval_secs: raw.get("gc_interval_secs")
                                .and_then(|v| v.as_u64()),
                            gc_disk_warning_threshold: raw.get("gc_disk_warning_threshold")
                                .and_then(|v| v.as_f64()),
                            health_check_interval_secs: raw.get("health_check_interval_secs")
                                .and_then(|v| v.as_u64()),
                            health_default_idle_timeout_secs: raw.get("health_default_idle_timeout_secs")
                                .and_then(|v| v.as_u64()),
                            logging_level: raw.get("logging_level")
                                .and_then(|v| v.as_str()).map(|s| s.to_string()),
                        };

                        if update.count_changes() == 0 {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({
                                    "error": "no_changes",
                                    "message": "No hot-reloadable fields were provided in the request body"
                                })),
                            );
                        }

                        match hot.apply_update(update.clone()).await {
                            Ok(()) => {
                                // If log level changed, apply it to the tracing subscriber
                                if let Some(ref level) = update.logging_level {
                                    if let Err(e) = log_h.update_levels(level) {
                                        tracing::warn!(error = %e, "failed to apply log level change via reload handle");
                                    } else {
                                        tracing::info!(new_level = %level, "log level changed at runtime");
                                    }
                                }

                                // Publish ConfigHotReload event to NATS (informational)
                                let changes_json = serde_json::to_value(&update)
                                    .unwrap_or(serde_json::json!({}));
                                let event = messaging::events::Event::ConfigHotReload {
                                    node_id: nid.clone(),
                                    changes: changes_json,
                                };
                                if let Err(e) = nbus.publish(&event).await {
                                    tracing::warn!(error = %e, "failed to publish ConfigHotReload event");
                                }

                                tracing::info!(
                                    changes = update.count_changes(),
                                    "hot config updated via admin API"
                                );

                                (
                                    axum::http::StatusCode::OK,
                                    axum::Json(serde_json::json!({
                                        "status": "updated",
                                        "changes_applied": update.count_changes(),
                                    })),
                                )
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "hot config update rejected");
                                (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "error": "validation_failed",
                                        "message": e.to_string(),
                                    })),
                                )
                            }
                        }
                    }
                }
            }),
        )
        .route(
            "/admin/config",
            axum::routing::delete({
                let hot = hot_config_handle.clone();
                move || {
                    let hot = hot.clone();
                    async move {
                        match hot.reset().await {
                            Ok(()) => {
                                tracing::info!("hot config reset to cold defaults via admin API");
                                (
                                    axum::http::StatusCode::OK,
                                    axum::Json(serde_json::json!({
                                        "status": "reset",
                                        "message": "Hot config reset to startup defaults. Restart to re-read TOML file.",
                                    })),
                                )
                            }
                            Err(e) => (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({
                                    "error": "reset_failed",
                                    "message": e.to_string(),
                                })),
                            ),
                        }
                    }
                }
            }),
        )
        // -- Logging Admin Endpoints ------------------------------------
        .route(
            "/admin/logging/levels",
            axum::routing::get({
                let _log_h = log_reload_handle.clone();
                move || {
                    let _log_h = _log_h.clone();
                    async move {
                        axum::Json(serde_json::json!({
                            "message": "Current log levels are managed by the tracing subscriber. \
                                        Use RUST_LOG format for updates.",
                            "hint": "PATCH /admin/logging/levels with {\"directives\": \"debug,supervisor=trace\"}"
                        }))
                    }
                }
            }),
        )
        .route(
            "/admin/logging/levels",
            axum::routing::patch({
                let log_h = log_reload_handle.clone();
                let nid = config.node.node_id.clone();
                move |body: axum::Json<serde_json::Value>| {
                    let log_h = log_h.clone();
                    let nid = nid.clone();
                    async move {
                        let directives = match body.0.get("directives").and_then(|v| v.as_str()) {
                            Some(d) => d,
                            None => {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "status": "error",
                                        "message": "Missing field 'directives' in request body",
                                    })),
                                );
                            }
                        };

                        match log_h.update_levels(directives) {
                            Ok(()) => {
                                tracing::info!(
                                    config_key = "logging.levels",
                                    new_value = %directives,
                                    node_id = %nid,
                                    "log levels updated via admin API"
                                );
                                (
                                    axum::http::StatusCode::OK,
                                    axum::Json(serde_json::json!({
                                        "status": "updated",
                                        "directives": directives,
                                    })),
                                )
                            }
                            Err(e) => {
                                (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({
                                        "status": "error",
                                        "message": format!("Invalid log directives: {}", e),
                                    })),
                                )
                            }
                        }
                    }
                }
            }),
        )
        // -- Token Rotation Endpoint ------------------------------------
        .route(
            "/admin/auth/rotate-token",
            axum::routing::post(move |body: axum::Json<serde_json::Value>| {
                let auth_config = rotate_auth_config.clone();
                let store = rotate_store.clone();
                let node_id = rotate_node_id.clone();
                async move {
                    if rotate_disabled {
                        return (
                            axum::http::StatusCode::CONFLICT,
                            axum::Json(serde_json::json!({
                                "error": "external_secret_lifecycle_required",
                                "message": "production token rotation must be performed in the external secret manager and applied through a controlled node reload"
                            })),
                        );
                    }
                    // Parse the request body
                    let req: proxy::auth_middleware::RotateTokenRequest = match serde_json::from_value(body.0) {
                        Ok(r) => r,
                        Err(e) => {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({
                                    "error": "invalid_request",
                                    "message": format!("Failed to parse request: {}", e)
                                })),
                            );
                        }
                    };

                    // Validate the rotation request
                    let new_token = match proxy::auth_middleware::validate_rotation_request(&req) {
                        Ok(t) => t,
                        Err(e) => {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({
                                    "error": "validation_failed",
                                    "message": e
                                })),
                            );
                        }
                    };

                    // Apply the rotation
                    let mut config = auth_config.write().await;
                    match req.token_type.as_str() {
                        "read" => {
                            config.read_token = Some(new_token.clone());
                            tracing::warn!(token_type = "read", "admin token rotated via admin API");
                        }
                        "write" => {
                            config.write_token = Some(new_token.clone());
                            tracing::warn!(token_type = "write", "admin token rotated via admin API");
                        }
                        _ => unreachable!("validate_rotation_request should have caught this"),
                    }

                    // Persist the updated config to redb
                    if let Err(e) = store.save_auth_config(&config) {
                        tracing::error!(error = %e, "failed to persist rotated token to database");
                    }

                    // Audit log the rotation
                    let audit_event = supervisor::audit::AuditEvent {
                        timestamp: chrono::Utc::now().timestamp_millis() as u64,
                        node_id,
                        event_type: supervisor::audit::AuditEventType::TokenRotated,
                        actor: "admin:write_token".to_string(),
                        app_id: "_platform".to_string(),
                        details: serde_json::json!({
                            "token_type": req.token_type,
                        }),
                    };
                    supervisor::audit::write_audit_event("/var/log/wasm-node/audit.jsonl", &audit_event);

                    drop(config);

                    (
                        axum::http::StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "status": "rotated",
                            "token_type": req.token_type,
                            "new_token": new_token,
                            "warning": "Save this token securely. It will not be shown again.",
                        })),
                    )
                }
            }),
        )
        // -- Auth Middleware Layer ---------------------------------------
        .layer(axum::middleware::from_fn_with_state(
            auth_state.clone(),
            proxy::auth_middleware::auth_middleware,
        ));

    let deploy_ingress_app = axum::Router::new()
        .route(
            "/deploy/intent",
            axum::routing::post({
                let store = store.clone();
                let secret_provider = secret_provider.clone();
                let artifact_server_url = artifact_server_url.clone();
                let artifact_transfer_authority = artifact_transfer_authority.clone();
                let node_id = config.node.node_id.clone();
                let cluster_node_stale_after_secs = config.health.cluster_node_stale_after_secs;
                let bus = bus.clone();
                move |axum::Json(body): axum::Json<common::deploy::DeployIntentRequest>| {
                    let store = store.clone();
                    let secret_provider = secret_provider.clone();
                    let artifact_server_url = artifact_server_url.clone();
                    let artifact_transfer_authority = artifact_transfer_authority.clone();
                    let node_id = node_id.clone();
                    let bus = bus.clone();
                    let cluster_node_stale_after_secs = cluster_node_stale_after_secs;
                    async move {
                        match handlers::process_deploy_intent(
                            handlers::DeployIntentContext {
                                store: &store,
                                secret_provider: secret_provider.as_ref(),
                                artifact_server_url: &artifact_server_url,
                                artifact_transfer_authority: &artifact_transfer_authority,
                                node_id: &node_id,
                                cluster_node_stale_after_secs,
                                bus: &bus,
                            },
                            body,
                        )
                        .await
                        {
                            Ok(response) => (
                                axum::http::StatusCode::ACCEPTED,
                                axum::Json(serde_json::to_value(response).unwrap_or_else(|e| {
                                    serde_json::json!({
                                        "error": "serialization_error",
                                        "message": format!("{e}"),
                                    })
                                })),
                            ),
                            Err(err) => {
                                let status = match &err {
                                    common::error::PlatformError::ConfigValidation(_) => {
                                        axum::http::StatusCode::BAD_REQUEST
                                    }
                                    common::error::PlatformError::Security(_) => {
                                        axum::http::StatusCode::FORBIDDEN
                                    }
                                    common::error::PlatformError::External { .. } => {
                                        axum::http::StatusCode::BAD_GATEWAY
                                    }
                                    common::error::PlatformError::Storage { .. }
                                    | common::error::PlatformError::Internal(_)
                                    | common::error::PlatformError::Io { .. } => {
                                        axum::http::StatusCode::INTERNAL_SERVER_ERROR
                                    }
                                    _ => axum::http::StatusCode::BAD_REQUEST,
                                };
                                (
                                    status,
                                    axum::Json(serde_json::json!({
                                        "error": "deploy_intent_failed",
                                        "message": format!("{err}"),
                                    })),
                                )
                            }
                        }
                    }
                }
            }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            auth_state.clone(),
            proxy::auth_middleware::auth_middleware,
        ));

    // -- Config Sync Loop ----------------------------------------------
    // Periodically reads HotConfigHandle and pushes updates to components
    // that need hot-reloadable parameters (rate limiter, eBPF, GC, health).
    {
        let sync_hot = hot_config_handle.clone();
        let sync_rate_limiter = rate_limiter_sync.clone();
        let sync_ebpf_dispatcher = ebpf_dispatcher_sync.clone();
        let sync_ebpf_handle = ebpf_handle.clone();
        let sync_gc_tx = gc_config_tx;
        let sync_health_tx = health_interval_tx;
        let sync_log_handle = log_reload_handle.clone();

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(10));
            loop {
                ticker.tick().await;
                let hot = sync_hot.read().await;

                // 1. Rate limiter
                sync_rate_limiter.update_default_config(proxy::rate_limiter::RateLimitConfig {
                    requests_per_second: hot.rate_limit.default_requests_per_second,
                    burst_capacity: hot.rate_limit.default_burst_capacity,
                    per_ip_limit: hot.rate_limit.default_per_ip_limit,
                });

                // 2. eBPF monitor thresholds
                let new_ebpf_config =
                    ebpf_monitor::MonitorConfig::from_ebpf_section(&common::config::EbpfSection {
                        enabled: hot.ebpf.enabled,
                        required: hot.ebpf.required,
                        fd_soft_limit: hot.ebpf.fd_soft_limit,
                        fd_hard_limit: hot.ebpf.fd_hard_limit,
                        mem_low_threshold_pages: hot.ebpf.mem_low_threshold_pages,
                        mem_critical_threshold_pages: hot.ebpf.mem_critical_threshold_pages,
                        disk_slow_threshold_ns: hot.ebpf.disk_slow_threshold_ns,
                        tcp_conn_limit_per_pid: hot.ebpf.tcp_conn_limit_per_pid,
                        syscall_rate_limit: hot.ebpf.syscall_rate_limit,
                        sampling_period_secs: hot.ebpf.sampling_period_secs,
                        enable_namespace_enforcer: hot.ebpf.enable_namespace_enforcer,
                        gateway_port: hot.ebpf.gateway_port,
                        enable_forged_header_detect: hot.ebpf.enable_forged_header_detect,
                    });
                sync_ebpf_dispatcher.update_thresholds(new_ebpf_config.clone());
                if let Err(error) = sync_ebpf_handle
                    .lock()
                    .await
                    .update_kernel_thresholds(&new_ebpf_config)
                {
                    tracing::warn!(%error, "failed to hot-reload eBPF kernel thresholds");
                }

                // 3. GC config (interval + disk threshold)
                let new_gc_config = common::gc::GcConfig {
                    artifact_keep_versions: hot.gc.artifact_keep_versions,
                    metrics_retain_days: hot.gc.metrics_retain_days,
                    undeploy_grace_secs: hot.gc.undeploy_grace_secs,
                    gc_interval_secs: hot.gc.gc_interval_secs,
                    disk_warning_threshold: hot.gc.disk_warning_threshold,
                };
                let _ = sync_gc_tx.send(new_gc_config);

                // 4. Health check interval
                let _ = sync_health_tx.send(hot.health.check_interval_secs);

                // 5. Log level (apply via reload handle if changed)
                if let Err(e) = sync_log_handle.update_levels(&hot.logging.level) {
                    tracing::debug!(error = %e, "config sync: log level unchanged or invalid");
                }
            }
        });
        info!("config sync loop started (10s interval, pushes hot-reload updates to components)");
    }

    // -- SIGHUP Handler for Auth Config Reload --------------------------
    // When the operator edits the config file with new tokens and sends
    // SIGHUP, the node reads the updated file and applies the new tokens
    // immediately. Old tokens are invalidated as soon as the new config
    // is loaded into the RwLock.
    setup_sighup_handler(auth_config_shared.clone(), args.config.clone());

    // -- Periodic Rate Limiter Pruning ----------------------------------
    // Prune stale IP buckets every 60 seconds to prevent memory leaks
    // on long-running nodes with many unique client IPs.
    {
        let prune_limiter = admin_rate_limiter.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            loop {
                ticker.tick().await;
                prune_limiter.prune_stale(Duration::from_secs(300)); // 5-minute idle threshold
            }
        });
    }

    let admin_addr = bind_socket_address(&config.admin.bind_address, config.admin.port)?;
    let admin_tls = admin_tls_material(&config);
    let (admin_tls_cert, admin_tls_key) = match admin_tls {
        Some((cert, key)) => (Some(cert), Some(key)),
        None => (None, None),
    };
    tokio::spawn(async move {
        if let Err(e) = serve_admin_app(admin_addr, admin_app, admin_tls_cert, admin_tls_key).await
        {
            tracing::error!(error = %e, "admin API server failed");
        }
    });

    let deploy_ingress_addr = bind_socket_address(
        &config.admin.deploy_ingress_bind_address,
        config.admin.deploy_ingress_port,
    )?;
    let deploy_ingress_tls = admin_tls_material(&config);
    let (deploy_ingress_tls_cert, deploy_ingress_tls_key) = match deploy_ingress_tls {
        Some((cert, key)) => (Some(cert), Some(key)),
        None => (None, None),
    };
    tokio::spawn(async move {
        if let Err(e) = serve_admin_app(
            deploy_ingress_addr,
            deploy_ingress_app,
            deploy_ingress_tls_cert,
            deploy_ingress_tls_key,
        )
        .await
        {
            tracing::error!(error = %e, "deploy ingress server failed");
        }
    });

    let mut artifact_peer_tokens: Vec<storage::artifact_server::ArtifactPeerTokenConfig> =
        Vec::new();
    if let Some(token) = effective_auth_config.write_token.clone() {
        if !artifact_peer_tokens
            .iter()
            .any(|existing| existing.token == token)
        {
            artifact_peer_tokens.push(storage::artifact_server::ArtifactPeerTokenConfig::new(
                token, None, true, true,
            ));
        }
    }
    let artifact_app = storage::artifact_server::artifact_router(
        store.clone(),
        artifact_peer_tokens,
        Some(artifact_transfer_authority.clone()),
    );
    let artifact_addr = bind_socket_address(
        &config.admin.artifact_bind_address,
        config.admin.artifact_port,
    )?;
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(&artifact_addr)
            .await
            .expect("artifact server bind failed");
        info!(addr = %artifact_addr, "artifact server listening");
        axum::serve(
            listener,
            artifact_app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    // Signal that startup is complete - all probes are now active
    startup_complete.store(true, std::sync::atomic::Ordering::Relaxed);
    info!(node_id = %config.node.node_id, "node startup complete - all probes active");

    info!(
        http = config.proxy.http_port,
        https = config.proxy.https_port,
        admin = config.admin.port,
        deploy_ingress = config.admin.deploy_ingress_port,
        artifact = config.admin.artifact_port,
        "node fully started"
    );

    // Run Pingora in background to allow graceful shutdown setup
    std::thread::spawn(move || {
        proxy_server.run();
    });

    // Wait for shutdown signal (SIGTERM / Ctrl-C)
    // On Linux/WSL, `systemctl stop` sends SIGTERM, so we must handle it.
    // We race both signals - whichever fires first triggers graceful drain.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

        tokio::select! {
            _ = sigterm.recv() => info!("SIGTERM received - gracefully shutting down all instances"),
            _ = sigint.recv() => info!("SIGINT (Ctrl-C) received - gracefully shutting down all instances"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.unwrap();
        info!("Ctrl-C received - gracefully shutting down all instances");
    }

    // Gracefully shutdown all instances with timeout
    let shutdown_timeout = std::time::Duration::from_secs(30);
    supervisor.shutdown_all(shutdown_timeout).await;

    info!("All instances stopped - exiting");
    std::process::exit(0);
}

use crate::config::Args;
use anyhow::Context;
use common::error::PlatformError;
use secrets::crypto::{decrypt, encrypt, EncryptedBlob, SymmetricKey};
use sha2::Digest;

pub fn encrypt_credential_value(kek_bytes: [u8; 32], value: &str) -> Result<String, PlatformError> {
    let key = SymmetricKey::from_bytes(kek_bytes);
    let encrypted = encrypt(&key, value.as_bytes())?;
    Ok(hex::encode(encrypted.0))
}

pub fn decrypt_credential_value(
    kek_bytes: [u8; 32],
    encrypted_hex: &str,
) -> Result<String, PlatformError> {
    let key = SymmetricKey::from_bytes(kek_bytes);
    let bytes = hex::decode(encrypted_hex)
        .map_err(|e| PlatformError::encryption_with_msg("invalid credential ciphertext hex", e))?;
    let plaintext = decrypt(&key, &EncryptedBlob(bytes))?;
    String::from_utf8(plaintext).map_err(|e| {
        PlatformError::encryption_with_msg("credential plaintext is not valid utf-8", e)
    })
}

pub fn load_kek(args: &Args) -> anyhow::Result<SymmetricKey> {
    match args.key_source.as_str() {
        "generate" => Ok(SymmetricKey::generate()),
        "file" => {
            let key_file = args
                .key_file
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("key_source=file requires --key-file"))?;
            let bytes = std::fs::read(key_file)
                .with_context(|| format!("failed to read key file {key_file}"))?;
            symm_key_from_exact_32(&bytes, &format!("key file {key_file}"))
        }
        spec if spec.starts_with("env:") => load_kek_from_env_spec(spec),
        spec if spec.starts_with("passphrase-env:") => {
            let passphrase = load_passphrase_from_env_spec(spec)?;
            let digest = sha2::Sha256::digest(passphrase.as_bytes());
            symm_key_from_exact_32(&digest[..32], "passphrase-env digest")
        }
        other => Err(anyhow::anyhow!(
            "unsupported key_source '{}'; supported values are generate, file, env:VAR, passphrase-env:VAR",
            other
        )),
    }
}

fn symm_key_from_exact_32(bytes: &[u8], source: &str) -> anyhow::Result<SymmetricKey> {
    if bytes.len() != 32 {
        anyhow::bail!(
            "{source} must contain exactly 32 bytes, found {} bytes",
            bytes.len()
        );
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(bytes);
    Ok(SymmetricKey::from_bytes(key))
}

fn load_kek_from_env_spec(spec: &str) -> anyhow::Result<SymmetricKey> {
    let var_name = spec
        .strip_prefix("env:")
        .ok_or_else(|| anyhow::anyhow!("invalid env key source: {spec}"))?;
    let raw = std::env::var(var_name)
        .map_err(|_| anyhow::anyhow!("environment variable {var_name} is not set"))?;
    let trimmed = raw.trim();
    if trimmed.len() == 64 {
        let decoded =
            hex::decode(trimmed).map_err(|e| anyhow::anyhow!("failed to decode hex KEK: {e}"))?;
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
    if raw.trim().is_empty() {
        anyhow::bail!("environment variable {var_name} must not be empty");
    }
    Ok(raw)
}

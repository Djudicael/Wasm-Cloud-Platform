use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use common::error::PlatformError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// A symmetric key (e.g. DEK or KEK).
/// - Zeroized on drop to prevent memory scraping.
/// - Does not implement Debug or Serialize to prevent accidental leakage.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SymmetricKey(pub [u8; 32]);

impl SymmetricKey {
    /// Generate a new random key.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut bytes);
        SymmetricKey(bytes)
    }

    /// Load from raw bytes (e.g. from env or TPM).
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        SymmetricKey(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Encrypted blob: nonce (12 bytes) || ciphertext.
pub struct EncryptedBlob(pub Vec<u8>);

/// Encrypt plaintext with AES-256-GCM.
/// Nonce is prepended to the ciphertext.
pub fn encrypt(key: &SymmetricKey, plaintext: &[u8]) -> Result<EncryptedBlob, PlatformError> {
    let cipher = Aes256Gcm::new_from_slice(key.as_bytes())
        .map_err(|e| PlatformError::Encryption(e.to_string()))?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| PlatformError::Encryption(e.to_string()))?;

    let mut blob = nonce.to_vec();
    blob.extend_from_slice(&ciphertext);
    Ok(EncryptedBlob(blob))
}

/// Decrypt an EncryptedBlob (nonce || ciphertext).
pub fn decrypt(key: &SymmetricKey, blob: &EncryptedBlob) -> Result<Vec<u8>, PlatformError> {
    if blob.0.len() < 12 {
        return Err(PlatformError::Encryption("blob too short".into()));
    }
    let (nonce_bytes, ciphertext) = blob.0.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);
    let cipher = Aes256Gcm::new_from_slice(key.as_bytes())
        .map_err(|e| PlatformError::Encryption(e.to_string()))?;
    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| PlatformError::Encryption(e.to_string()))
}

/// Per-app encrypted secret bundle stored in redb.
#[derive(Debug, Serialize, Deserialize)]
pub struct AppSecretBundle {
    pub app_id: String,
    /// The DEK encrypted with the node's KEK.
    pub encrypted_dek: Vec<u8>,
    /// Map of secret_key → value encrypted with the DEK.
    pub secrets: HashMap<String, Vec<u8>>,
    /// Monotonically increasing version for rotation tracking.
    pub version: u64,
    /// Unix timestamp of last update.
    pub updated_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symmetric_key_generate() {
        let key1 = SymmetricKey::generate();
        let key2 = SymmetricKey::generate();
        assert_ne!(key1.as_bytes(), key2.as_bytes());
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = SymmetricKey::generate();
        for i in 0..10 {
            let plaintext = format!("test_secret_data_{}", i).into_bytes();
            let encrypted = encrypt(&key, &plaintext).unwrap();
            assert!(encrypted.0.len() >= 13); // nonce is 12 + min ciphertext

            let decrypted = decrypt(&key, &encrypted).unwrap();
            assert_eq!(decrypted, plaintext);
        }
    }

    #[test]
    fn test_decrypt_wrong_key() {
        let key1 = SymmetricKey::generate();
        let key2 = SymmetricKey::generate();

        let plaintext = b"super_secret_data";
        let encrypted = encrypt(&key1, plaintext).unwrap();

        let result = decrypt(&key2, &encrypted);
        assert!(result.is_err());
    }
}

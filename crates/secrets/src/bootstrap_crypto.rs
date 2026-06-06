// crates/secrets/src/bootstrap_crypto.rs
//! X25519 key exchange for secure cluster bootstrap secret transfer.
//!
//! When a new node joins, it generates an ephemeral X25519 keypair and sends
//! the public key in the NodeJoined event. The existing node encrypts secrets
//! using this public key (via ECDH + ChaCha20Poly1305) so that only the new
//! node can decrypt them.

use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    ChaCha20Poly1305, Nonce,
};
use common::error::PlatformError;
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};

/// X25519 keypair for bootstrap secret transfer.
/// Uses StaticSecret (not Ephemeral) so we can decrypt multiple times.
pub struct BootstrapKeyPair {
    secret: StaticSecret,
    pub public: PublicKey,
}

impl BootstrapKeyPair {
    /// Generate a new keypair for this bootstrap session.
    pub fn generate() -> Self {
        let secret = StaticSecret::from(rand::random::<[u8; 32]>());
        let public = PublicKey::from(&secret);
        BootstrapKeyPair { secret, public }
    }

    pub fn from_secret_bytes(secret_bytes: [u8; 32]) -> Self {
        let secret = StaticSecret::from(secret_bytes);
        let public = PublicKey::from(&secret);
        BootstrapKeyPair { secret, public }
    }

    /// Decrypt ciphertext that was encrypted FOR our public key.
    /// The ciphertext format is: [ephemeral_pubkey(32) | nonce(12) | ciphertext]
    ///
    /// Returns an error if the ciphertext is malformed or decryption fails,
    /// rather than silently returning an empty vector.
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, PlatformError> {
        if ciphertext.len() < 32 + 12 {
            return Err(PlatformError::encryption(format!(
                "Ciphertext too short: got {} bytes, need at least 44",
                ciphertext.len()
            )));
        }

        // Extract ephemeral public key from first 32 bytes
        let mut ephemeral_pubkey_bytes = [0u8; 32];
        ephemeral_pubkey_bytes.copy_from_slice(&ciphertext[..32]);
        let ephemeral_public = PublicKey::from(ephemeral_pubkey_bytes);

        // Derive shared secret using our private key and their ephemeral public key
        let shared = self.secret.diffie_hellman(&ephemeral_public);
        let cipher = ChaCha20Poly1305::new(shared.as_bytes().into());

        // Extract nonce and ciphertext
        let nonce = Nonce::from_slice(&ciphertext[32..44]);
        cipher
            .decrypt(nonce, &ciphertext[44..])
            .map_err(|e| PlatformError::encryption(format!("Decryption failed: {}", e)))
    }

    /// Get public key as bytes for transmission.
    pub fn public_bytes(&self) -> Vec<u8> {
        self.public.as_bytes().to_vec()
    }

    pub fn secret_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }
}

/// Encrypt plaintext for a peer's public key.
/// Uses ephemeral ECDH + ChaCha20Poly1305.
/// Output format: [ephemeral_pubkey(32) | nonce(12) | ciphertext]
///
/// Returns an error if the peer public key is invalid or encryption fails,
/// rather than silently returning an empty vector.
pub fn encrypt_for_peer(
    peer_public_bytes: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, PlatformError> {
    let mut rng = OsRng;
    let ephemeral = EphemeralSecret::random_from_rng(rng);
    let ephemeral_public = PublicKey::from(&ephemeral);

    let peer_public =
        x25519_dalek::PublicKey::from(*<&[u8; 32]>::try_from(peer_public_bytes).map_err(|_| {
            PlatformError::encryption("Invalid peer public key length: expected 32 bytes")
        })?);

    let shared = ephemeral.diffie_hellman(&peer_public);
    let cipher = ChaCha20Poly1305::new(shared.as_bytes().into());
    let nonce = ChaCha20Poly1305::generate_nonce(&mut rng);

    // Prepend ephemeral public key so receiver can derive the shared secret
    let mut out = ephemeral_public.as_bytes().to_vec();
    out.extend(nonce.to_vec());
    let ciphertext = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| PlatformError::encryption(format!("Encryption failed: {}", e)))?;
    out.extend(ciphertext);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bootstrap_encryption() {
        let keypair = BootstrapKeyPair::generate();
        let plaintext = b"secret_value_12345";

        let ciphertext = encrypt_for_peer(keypair.public.as_bytes(), plaintext).unwrap();
        assert!(!ciphertext.is_empty());
        assert_ne!(&ciphertext[..], plaintext);

        let decrypted = keypair.decrypt(&ciphertext).unwrap();
        assert_eq!(&decrypted, plaintext);
    }

    #[test]
    fn test_multiple_messages() {
        let receiver = BootstrapKeyPair::generate();

        let msg1 = b"first secret";
        let msg2 = b"second secret value";

        let cipher1 = encrypt_for_peer(receiver.public.as_bytes(), msg1).unwrap();
        let cipher2 = encrypt_for_peer(receiver.public.as_bytes(), msg2).unwrap();

        // Each encryption uses different ephemeral key
        assert_ne!(&cipher1[..32], &cipher2[..32]);

        let plain1 = receiver.decrypt(&cipher1).unwrap();
        let plain2 = receiver.decrypt(&cipher2).unwrap();

        assert_eq!(&plain1, msg1);
        assert_eq!(&plain2, msg2);
    }

    #[test]
    fn test_decrypt_too_short_returns_error() {
        let keypair = BootstrapKeyPair::generate();
        let short_ciphertext = vec![0u8; 43]; // 44 bytes minimum
        let result = keypair.decrypt(&short_ciphertext);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("too short"));
    }

    #[test]
    fn test_encrypt_invalid_public_key_returns_error() {
        let bad_pubkey = vec![0u8; 31]; // wrong length
        let result = encrypt_for_peer(&bad_pubkey, b"test");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Invalid peer public key length"));
    }

    #[test]
    fn test_decrypt_corrupted_ciphertext_returns_error() {
        let keypair = BootstrapKeyPair::generate();
        let plaintext = b"test_data";

        let mut ciphertext = encrypt_for_peer(keypair.public.as_bytes(), plaintext).unwrap();
        // Corrupt the ciphertext portion (after the 44-byte header)
        if ciphertext.len() > 45 {
            ciphertext[45] ^= 0xFF;
        }
        let result = keypair.decrypt(&ciphertext);
        assert!(result.is_err());
    }
}

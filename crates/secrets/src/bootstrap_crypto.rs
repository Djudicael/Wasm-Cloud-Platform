// crates/secrets/src/bootstrap_crypto.rs
//! X25519 key exchange for secure cluster bootstrap secret transfer.
//!
//! When a new node joins, it generates an ephemeral X25519 keypair and sends
//! the public key in the NodeJoined event. The existing node encrypts secrets
//! using this public key (via ECDH + ChaCha20Poly1305) so that only the new
//! node can decrypt them.

use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use rand::rngs::OsRng;
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
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        BootstrapKeyPair { secret, public }
    }

    /// Decrypt ciphertext that was encrypted FOR our public key.
    /// The ciphertext format is: [ephemeral_pubkey(32) | nonce(12) | ciphertext]
    /// Returns empty vec on failure.
    pub fn decrypt(&self, ciphertext: &[u8]) -> Vec<u8> {
        if ciphertext.len() < 32 + 12 {
            return Vec::new();
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
        cipher.decrypt(nonce, &ciphertext[44..]).unwrap_or_default()
    }

    /// Get public key as bytes for transmission.
    pub fn public_bytes(&self) -> Vec<u8> {
        self.public.as_bytes().to_vec()
    }
}

/// Encrypt plaintext for a peer's public key.
/// Uses ephemeral ECDH + ChaCha20Poly1305.
/// Output format: [ephemeral_pubkey(32) | nonce(12) | ciphertext]
pub fn encrypt_for_peer(peer_public_bytes: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let ephemeral = EphemeralSecret::random_from_rng(OsRng);
    let ephemeral_public = PublicKey::from(&ephemeral);

    let peer_public = match <[u8; 32]>::try_from(peer_public_bytes) {
        Ok(arr) => PublicKey::from(arr),
        Err(_) => return Vec::new(), // Invalid public key
    };
    let shared = ephemeral.diffie_hellman(&peer_public);
    let cipher = ChaCha20Poly1305::new(shared.as_bytes().into());
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);

    // Prepend ephemeral public key so receiver can derive the shared secret
    let mut out = ephemeral_public.as_bytes().to_vec();
    out.extend(nonce.to_vec());
    match cipher.encrypt(&nonce, plaintext) {
        Ok(ciphertext) => out.extend(ciphertext),
        Err(_) => return Vec::new(),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bootstrap_encryption() {
        let keypair = BootstrapKeyPair::generate();
        let plaintext = b"secret_value_12345";

        let ciphertext = encrypt_for_peer(keypair.public.as_bytes(), plaintext);
        assert!(!ciphertext.is_empty());
        assert_ne!(&ciphertext[..], plaintext);

        let decrypted = keypair.decrypt(&ciphertext);
        assert_eq!(&decrypted, plaintext);
    }

    #[test]
    fn test_multiple_messages() {
        let receiver = BootstrapKeyPair::generate();

        let msg1 = b"first secret";
        let msg2 = b"second secret value";

        let cipher1 = encrypt_for_peer(receiver.public.as_bytes(), msg1);
        let cipher2 = encrypt_for_peer(receiver.public.as_bytes(), msg2);

        // Each encryption uses different ephemeral key
        assert_ne!(&cipher1[..32], &cipher2[..32]);

        let plain1 = receiver.decrypt(&cipher1);
        let plain2 = receiver.decrypt(&cipher2);

        assert_eq!(&plain1, msg1);
        assert_eq!(&plain2, msg2);
    }
}

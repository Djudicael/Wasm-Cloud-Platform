use serde::{Deserialize, Serialize};

/// Canonical transport envelope for secret updates moving across subsystem
/// boundaries such as ctl -> NATS -> node or bootstrap snapshot transfer.
///
/// The payload variant makes the transport encoding explicit so the receiver can
/// reject unexpected secret formats instead of inferring them from raw bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretTransportEnvelope {
    pub version: u32,
    pub payload: SecretTransportPayload,
}

impl SecretTransportEnvelope {
    pub const VERSION_1: u32 = 1;

    pub fn plaintext_utf8(value: impl Into<String>) -> Self {
        Self {
            version: Self::VERSION_1,
            payload: SecretTransportPayload::PlaintextUtf8V1 {
                value: value.into(),
            },
        }
    }

    pub fn bootstrap_peer_ciphertext(ciphertext: Vec<u8>) -> Self {
        Self {
            version: Self::VERSION_1,
            payload: SecretTransportPayload::BootstrapPeerCiphertextV1 { ciphertext },
        }
    }

    pub fn node_transport_ciphertext(ciphertext: Vec<u8>) -> Self {
        Self {
            version: Self::VERSION_1,
            payload: SecretTransportPayload::NodeTransportCiphertextV1 { ciphertext },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SecretTransportPayload {
    /// Development-compatible plaintext transport used for operator-driven
    /// secret rotation until a stronger cluster secret-distribution design is
    /// introduced.
    PlaintextUtf8V1 { value: String },
    /// Normal steady-state secret rotation encrypted for a specific node's
    /// long-lived transport public key.
    NodeTransportCiphertextV1 { ciphertext: Vec<u8> },
    /// Bootstrap secret transfer encrypted for a specific joining node using
    /// its one-time bootstrap public key.
    BootstrapPeerCiphertextV1 { ciphertext: Vec<u8> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretTransportEntry {
    pub app_id: String,
    pub key: String,
    pub envelope: SecretTransportEnvelope,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plaintext_envelope_constructor() {
        let envelope = SecretTransportEnvelope::plaintext_utf8("super-secret");
        assert_eq!(envelope.version, SecretTransportEnvelope::VERSION_1);
        assert_eq!(
            envelope.payload,
            SecretTransportPayload::PlaintextUtf8V1 {
                value: "super-secret".to_string()
            }
        );
    }

    #[test]
    fn test_bootstrap_ciphertext_constructor() {
        let ciphertext = vec![1, 2, 3, 4];
        let envelope = SecretTransportEnvelope::bootstrap_peer_ciphertext(ciphertext.clone());
        assert_eq!(envelope.version, SecretTransportEnvelope::VERSION_1);
        assert_eq!(
            envelope.payload,
            SecretTransportPayload::BootstrapPeerCiphertextV1 { ciphertext }
        );
    }

    #[test]
    fn test_node_transport_ciphertext_constructor() {
        let ciphertext = vec![9, 8, 7, 6];
        let envelope = SecretTransportEnvelope::node_transport_ciphertext(ciphertext.clone());
        assert_eq!(envelope.version, SecretTransportEnvelope::VERSION_1);
        assert_eq!(
            envelope.payload,
            SecretTransportPayload::NodeTransportCiphertextV1 { ciphertext }
        );
    }
}

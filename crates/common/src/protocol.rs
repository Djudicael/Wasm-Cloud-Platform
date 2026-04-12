// crates/common/src/protocol.rs
use serde::{Deserialize, Serialize};

/// Current protocol version of this binary.
/// Increment when NATS message format introduces a breaking change.
///
/// Version History:
/// - 1: Initial protocol (messages, events, cluster communication)
pub const PROTOCOL_VERSION: u32 = 1;

/// Minimum protocol version this binary can communicate with.
/// Nodes running a version below this are incompatible and should be upgraded first.
pub const MIN_COMPATIBLE_PROTOCOL: u32 = 1;

/// Binary version string (semantic versioning).
/// This is separate from protocol version - multiple binary versions can share the same protocol.
pub const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Every NATS message is wrapped in this envelope.
/// This provides protocol versioning and message metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEnvelope<T> {
    /// Protocol version of the sender.
    pub protocol_version: u32,

    /// Node ID of the sender.
    pub sender: String,

    /// Timestamp (milliseconds since UNIX epoch).
    pub timestamp_ms: u64,

    /// The actual event payload.
    pub payload: T,
}

impl<T: Serialize> MessageEnvelope<T> {
    pub fn new(sender: &str, payload: T) -> Self {
        MessageEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sender: sender.to_string(),
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            payload,
        }
    }

    /// Check if this message's protocol version is compatible with ours.
    pub fn is_compatible(&self) -> bool {
        self.protocol_version >= MIN_COMPATIBLE_PROTOCOL
            && self.protocol_version <= PROTOCOL_VERSION + 1
    }

    /// Get a human-readable compatibility status.
    pub fn compatibility_status(&self) -> CompatibilityStatus {
        if self.protocol_version < MIN_COMPATIBLE_PROTOCOL {
            CompatibilityStatus::TooOld {
                message_version: self.protocol_version,
                min_supported: MIN_COMPATIBLE_PROTOCOL,
            }
        } else if self.protocol_version > PROTOCOL_VERSION + 1 {
            CompatibilityStatus::TooNew {
                message_version: self.protocol_version,
                current_version: PROTOCOL_VERSION,
            }
        } else {
            CompatibilityStatus::Compatible
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatibilityStatus {
    Compatible,
    TooOld {
        message_version: u32,
        min_supported: u32,
    },
    TooNew {
        message_version: u32,
        current_version: u32,
    },
}

impl std::fmt::Display for CompatibilityStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompatibilityStatus::Compatible => write!(f, "compatible"),
            CompatibilityStatus::TooOld {
                message_version,
                min_supported,
            } => write!(
                f,
                "protocol v{} too old (minimum supported: v{})",
                message_version, min_supported
            ),
            CompatibilityStatus::TooNew {
                message_version,
                current_version,
            } => write!(
                f,
                "protocol v{} too new (current: v{}, max gap: 1)",
                message_version, current_version
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_envelope_creation() {
        let payload = "test message";
        let envelope = MessageEnvelope::new("node-0", payload);

        assert_eq!(envelope.protocol_version, PROTOCOL_VERSION);
        assert_eq!(envelope.sender, "node-0");
        assert_eq!(envelope.payload, "test message");
        assert!(envelope.timestamp_ms > 0);
    }

    #[test]
    fn test_compatibility_same_version() {
        let envelope = MessageEnvelope {
            protocol_version: PROTOCOL_VERSION,
            sender: "node-0".to_string(),
            timestamp_ms: 1000,
            payload: (),
        };

        assert!(envelope.is_compatible());
        assert_eq!(
            envelope.compatibility_status(),
            CompatibilityStatus::Compatible
        );
    }

    #[test]
    fn test_compatibility_one_version_ahead() {
        let envelope = MessageEnvelope {
            protocol_version: PROTOCOL_VERSION + 1,
            sender: "node-0".to_string(),
            timestamp_ms: 1000,
            payload: (),
        };

        assert!(envelope.is_compatible());
        assert_eq!(
            envelope.compatibility_status(),
            CompatibilityStatus::Compatible
        );
    }

    #[test]
    fn test_compatibility_too_new() {
        let envelope = MessageEnvelope {
            protocol_version: PROTOCOL_VERSION + 2,
            sender: "node-0".to_string(),
            timestamp_ms: 1000,
            payload: (),
        };

        assert!(!envelope.is_compatible());
        assert!(matches!(
            envelope.compatibility_status(),
            CompatibilityStatus::TooNew { .. }
        ));
    }

    #[test]
    fn test_compatibility_too_old() {
        let envelope = MessageEnvelope {
            protocol_version: MIN_COMPATIBLE_PROTOCOL - 1,
            sender: "node-0".to_string(),
            timestamp_ms: 1000,
            payload: (),
        };

        assert!(!envelope.is_compatible());
        assert!(matches!(
            envelope.compatibility_status(),
            CompatibilityStatus::TooOld { .. }
        ));
    }

    #[test]
    fn test_binary_version_format() {
        // Verify BINARY_VERSION is set from Cargo.toml
        assert!(!BINARY_VERSION.is_empty());
        // Should match semver pattern (e.g., "0.1.0")
        let parts: Vec<&str> = BINARY_VERSION.split('.').collect();
        assert!(parts.len() >= 2, "Version should have at least major.minor");
    }
}

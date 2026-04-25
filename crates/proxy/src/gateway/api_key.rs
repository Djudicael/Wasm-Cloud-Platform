use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// API key validator for endpoint-level authentication.
/// Stores SHA-256 hashed keys and validates incoming X-Api-Key headers.
#[derive(Debug, Clone, Default)]
pub struct ApiKeyValidator {
    keys: HashMap<String, common::types::ApiKeyRecord>, // key_hash → record
}

impl ApiKeyValidator {
    pub fn new(records: Vec<common::types::ApiKeyRecord>) -> Self {
        let mut keys = HashMap::new();
        for record in records {
            keys.insert(record.key_hash.clone(), record);
        }
        ApiKeyValidator { keys }
    }

    /// Validate an API key against the stored hashes.
    /// Returns true if the key is valid and has scope for the given path.
    pub fn validate(&self, header_value: &str, path: &str) -> bool {
        let mut hasher = Sha256::new();
        hasher.update(header_value);
        let hash = format!("sha256${}", hex::encode(hasher.finalize()));

        if let Some(record) = self.keys.get(&hash) {
            return record.scopes.is_empty()
                || record.scopes.iter().any(|s| path.starts_with(s));
        }
        false
    }

    /// Check if any API keys are configured.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_api_key_validation() {
        let mut hasher = Sha256::new();
        hasher.update("secret-key-123");
        let hash = format!("sha256${}", hex::encode(hasher.finalize()));

        let record = common::types::ApiKeyRecord {
            name: "test-key".to_string(),
            key_hash: hash,
            scopes: vec!["/api/public".to_string()],
        };

        let validator = ApiKeyValidator::new(vec![record]);

        assert!(validator.validate("secret-key-123", "/api/public/users"));
        assert!(!validator.validate("secret-key-123", "/api/admin"));
        assert!(!validator.validate("wrong-key", "/api/public"));
    }

    #[test]
    fn test_api_key_no_scopes() {
        let mut hasher = Sha256::new();
        hasher.update("global-key");
        let hash = format!("sha256${}", hex::encode(hasher.finalize()));

        let record = common::types::ApiKeyRecord {
            name: "global".to_string(),
            key_hash: hash,
            scopes: vec![],
        };

        let validator = ApiKeyValidator::new(vec![record]);
        assert!(validator.validate("global-key", "/any/path"));
    }
}

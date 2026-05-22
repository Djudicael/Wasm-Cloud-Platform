//! Authentication configuration and types for the admin API.
//!
//! This module implements bearer-token authentication with separate read/write
//! permission levels, constant-time token comparison, and token generation.
//!
//! # Security Model
//!
//! - **Read token**: Grants access to GET endpoints only (monitoring, status)
//! - **Write token**: Grants access to all endpoints including mutations
//! - **No token** (auth disabled): Everyone gets write access (backward compatible)
//!
//! # Token Format
//!
//! Tokens are 32-byte (64-character) hex strings generated with `OsRng`.
//! Minimum accepted length is 16 characters for operator convenience.

use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

pub use ipnet::IpNet as TrustedProxyNet;

/// Authentication configuration for the admin API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Enable authentication on admin API endpoints.
    /// When disabled, all endpoints are accessible without a token.
    /// WARNING: Disabling auth in production is a security risk.
    #[serde(default)]
    pub enabled: bool,

    /// Read-only bearer token. Grants access to GET endpoints:
    /// /status/*, /admin/config (GET), /health, /metrics
    #[serde(default)]
    pub read_token: Option<String>,

    /// Read-write bearer token. Grants access to all endpoints including:
    /// /admin/rebuild, /admin/gc/force, /admin/config (PATCH/DELETE)
    #[serde(default)]
    pub write_token: Option<String>,

    /// Require TLS for admin API when authentication is enabled.
    /// If true and the admin API is on HTTP (not HTTPS), the node refuses to start.
    /// Set to false for development environments.
    #[serde(default = "default_require_tls")]
    pub require_tls: bool,

    /// Rate limit for admin API requests (requests per second per IP).
    /// Set to 0 to disable rate limiting.
    #[serde(default = "default_admin_rate_limit")]
    pub rate_limit_per_second: u32,

    /// Maximum burst for admin API rate limiting.
    #[serde(default = "default_admin_burst")]
    pub rate_limit_burst: u32,

    /// Trusted proxy IPs/CIDR ranges allowed to supply forwarded client IP
    /// headers for the admin API. When empty, `X-Forwarded-For` and
    /// `X-Real-IP` are ignored and the direct peer socket address is used.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
}

fn default_require_tls() -> bool {
    true
}
fn default_admin_rate_limit() -> u32 {
    10
}
fn default_admin_burst() -> u32 {
    20
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            enabled: false, // Off by default for backward compatibility
            read_token: None,
            write_token: None,
            require_tls: true,
            rate_limit_per_second: default_admin_rate_limit(),
            rate_limit_burst: default_admin_burst(),
            trusted_proxies: Vec::new(),
        }
    }
}

/// Permission levels for admin API access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Permission {
    /// No access — authentication failed.
    None = 0,
    /// Read-only access: GET endpoints only.
    Read = 1,
    /// Read-write access: all endpoints.
    Write = 2,
}

/// Result of authenticating a request.
#[derive(Debug)]
pub struct AuthResult {
    pub permission: Permission,
    pub token_type: TokenType,
}

/// Identifies which token was used for the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    None,
    ReadToken,
    WriteToken,
}

impl std::fmt::Display for TokenType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenType::None => write!(f, "none"),
            TokenType::ReadToken => write!(f, "read_token"),
            TokenType::WriteToken => write!(f, "write_token"),
        }
    }
}

impl AuthConfig {
    pub fn trusted_proxy_nets(&self) -> Result<Vec<TrustedProxyNet>, String> {
        self.trusted_proxies
            .iter()
            .map(|entry| parse_trusted_proxy_entry(entry))
            .collect()
    }

    /// Authenticate a request's Authorization header.
    ///
    /// Returns the granted permission level and which token type was matched.
    /// When auth is disabled, everyone gets Write access (backward compatible).
    pub fn authenticate(&self, auth_header: Option<&str>) -> AuthResult {
        if !self.enabled {
            // Auth disabled: everyone gets write access (backward compatible)
            return AuthResult {
                permission: Permission::Write,
                token_type: TokenType::WriteToken,
            };
        }

        let token = match Self::extract_bearer_token(auth_header) {
            Some(t) => t,
            None => {
                return AuthResult {
                    permission: Permission::None,
                    token_type: TokenType::None,
                };
            }
        };

        // Check write token first (higher privilege)
        if let Some(ref write) = self.write_token {
            if crate::crypto::constant_time_eq(token.as_bytes(), write.as_bytes()) {
                return AuthResult {
                    permission: Permission::Write,
                    token_type: TokenType::WriteToken,
                };
            }
        }

        // Check read token
        if let Some(ref read) = self.read_token {
            if crate::crypto::constant_time_eq(token.as_bytes(), read.as_bytes()) {
                return AuthResult {
                    permission: Permission::Read,
                    token_type: TokenType::ReadToken,
                };
            }
        }

        AuthResult {
            permission: Permission::None,
            token_type: TokenType::None,
        }
    }

    /// Extract the bearer token from an Authorization header value.
    ///
    /// Expected format: `Bearer <token>`
    /// Returns `None` if the header is missing or malformed.
    fn extract_bearer_token(header: Option<&str>) -> Option<String> {
        let header = header?;
        let prefix = "Bearer ";
        if header.starts_with(prefix) {
            let token = header[prefix.len()..].trim().to_string();
            if token.is_empty() {
                None
            } else {
                Some(token)
            }
        } else {
            None
        }
    }

    /// Validate the auth configuration at startup.
    ///
    /// Returns an error if the configuration is invalid or insecure:
    /// - Auth enabled but no tokens configured
    /// - Read and write tokens are identical
    /// - Tokens shorter than 16 characters
    /// - Rate limit unreasonably high
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }

        // At least one token must be set
        if self.read_token.is_none() && self.write_token.is_none() {
            return Err("auth is enabled but no tokens are configured. \
                 Set auth.read_token and/or auth.write_token in config."
                .to_string());
        }

        // Tokens must be different
        if let (Some(ref read), Some(ref write)) = (&self.read_token, &self.write_token) {
            if read == write {
                return Err("auth.read_token and auth.write_token must be different".to_string());
            }
        }

        // Token minimum length (security: short tokens are brute-forceable)
        if let Some(ref token) = self.read_token {
            if token.len() < 16 {
                return Err(format!(
                    "auth.read_token is too short ({} chars, minimum 16)",
                    token.len()
                ));
            }
        }
        if let Some(ref token) = self.write_token {
            if token.len() < 16 {
                return Err(format!(
                    "auth.write_token is too short ({} chars, minimum 16)",
                    token.len()
                ));
            }
        }

        // Rate limit must be reasonable
        if self.rate_limit_per_second > 1000 {
            return Err(format!(
                "auth.rate_limit_per_second is too high ({}, maximum 1000)",
                self.rate_limit_per_second
            ));
        }

        if self.rate_limit_burst > 0 && self.rate_limit_per_second == 0 {
            // burst set but rate is 0 (disabled) — warn but allow
        }

        self.trusted_proxy_nets()?;

        Ok(())
    }

    /// Generate a new random token suitable for use as a bearer token.
    ///
    /// Returns a 32-byte hex string (64 characters) generated with `OsRng`.
    pub fn generate_token() -> String {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        hex::encode(bytes)
    }

    /// Generate a default auth config with random tokens.
    ///
    /// Used by `wasm-node --generate-tokens` or `--generate-config`.
    pub fn generate_default() -> Self {
        AuthConfig {
            enabled: true,
            read_token: Some(Self::generate_token()),
            write_token: Some(Self::generate_token()),
            require_tls: true,
            rate_limit_per_second: 10,
            rate_limit_burst: 20,
            trusted_proxies: Vec::new(),
        }
    }

    /// Create an AuthConfig from a legacy single admin token.
    ///
    /// This provides backward compatibility: if the operator only set
    /// `admin.auth_token`, it is treated as the write token with auth enabled.
    ///
    /// This path intentionally preserves legacy local-only behavior by leaving
    /// `require_tls = false`. Production guidance should use the structured
    /// `[auth]` section instead of relying on `admin.auth_token`.
    pub fn from_legacy_token(token: &str) -> Self {
        AuthConfig {
            enabled: true,
            read_token: None,
            write_token: Some(token.to_string()),
            require_tls: false, // Legacy mode: don't enforce TLS
            rate_limit_per_second: 10,
            rate_limit_burst: 20,
            trusted_proxies: Vec::new(),
        }
    }
}

fn parse_trusted_proxy_entry(entry: &str) -> Result<IpNet, String> {
    let trimmed = entry.trim();
    if trimmed.is_empty() {
        return Err("auth.trusted_proxies entries must not be empty".to_string());
    }

    trimmed
        .parse::<IpNet>()
        .or_else(|_| trimmed.parse::<IpAddr>().map(IpNet::from).map_err(|_| ()))
        .map_err(|_| format!("auth.trusted_proxies entry '{trimmed}' is not a valid IP or CIDR"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_disabled_grants_write() {
        let config = AuthConfig::default(); // enabled = false
        let result = config.authenticate(None);
        assert_eq!(result.permission, Permission::Write);
        assert_eq!(result.token_type, TokenType::WriteToken);
    }

    #[test]
    fn test_auth_valid_write_token() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            read_token: None,
            ..Default::default()
        };
        let result = config.authenticate(Some("Bearer write_token_1234567890"));
        assert_eq!(result.permission, Permission::Write);
        assert_eq!(result.token_type, TokenType::WriteToken);
    }

    #[test]
    fn test_auth_valid_read_token() {
        let config = AuthConfig {
            enabled: true,
            read_token: Some("read_token_1234567890".to_string()),
            write_token: None,
            ..Default::default()
        };
        let result = config.authenticate(Some("Bearer read_token_1234567890"));
        assert_eq!(result.permission, Permission::Read);
        assert_eq!(result.token_type, TokenType::ReadToken);
    }

    #[test]
    fn test_auth_invalid_token() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            read_token: Some("read_token_1234567890".to_string()),
            ..Default::default()
        };
        let result = config.authenticate(Some("Bearer wrong_token_value"));
        assert_eq!(result.permission, Permission::None);
    }

    #[test]
    fn test_auth_missing_header() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            ..Default::default()
        };
        let result = config.authenticate(None);
        assert_eq!(result.permission, Permission::None);
    }

    #[test]
    fn test_auth_empty_bearer() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            ..Default::default()
        };
        let result = config.authenticate(Some("Bearer "));
        assert_eq!(result.permission, Permission::None);
    }

    #[test]
    fn test_auth_malformed_header() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            ..Default::default()
        };
        let result = config.authenticate(Some("Basic dXNlcjpwYXNz"));
        assert_eq!(result.permission, Permission::None);
    }

    #[test]
    fn test_auth_write_token_has_read_access() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("write_token_1234567890".to_string()),
            read_token: Some("read_token_1234567890".to_string()),
            ..Default::default()
        };
        let result = config.authenticate(Some("Bearer write_token_1234567890"));
        assert!(result.permission >= Permission::Read);
        assert!(result.permission >= Permission::Write);
    }

    #[test]
    fn test_permission_ordering() {
        assert!(Permission::Write > Permission::Read);
        assert!(Permission::Read > Permission::None);
        assert!(Permission::Write > Permission::None);
    }

    #[test]
    fn test_extract_bearer_token_valid() {
        let result = AuthConfig::extract_bearer_token(Some("Bearer abc123"));
        assert_eq!(result, Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_bearer_token_with_whitespace() {
        let result = AuthConfig::extract_bearer_token(Some("Bearer   abc123  "));
        assert_eq!(result, Some("abc123".to_string()));
    }

    #[test]
    fn test_extract_bearer_token_none() {
        let result = AuthConfig::extract_bearer_token(None);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_bearer_token_wrong_scheme() {
        let result = AuthConfig::extract_bearer_token(Some("Basic abc123"));
        assert!(result.is_none());
    }

    #[test]
    fn test_validate_no_tokens_when_enabled() {
        let config = AuthConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_same_tokens() {
        let config = AuthConfig {
            enabled: true,
            read_token: Some("same_token_1234567890".to_string()),
            write_token: Some("same_token_1234567890".to_string()),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_short_read_token() {
        let config = AuthConfig {
            enabled: true,
            read_token: Some("short".to_string()),
            write_token: Some("long_enough_write_token".to_string()),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_short_write_token() {
        let config = AuthConfig {
            enabled: true,
            read_token: Some("long_enough_read_token".to_string()),
            write_token: Some("short".to_string()),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_high_rate_limit() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("valid_write_token_here".to_string()),
            rate_limit_per_second: 5000,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_disabled_is_always_ok() {
        let config = AuthConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_valid_config() {
        let config = AuthConfig {
            enabled: true,
            read_token: Some("a_valid_read_token_1234".to_string()),
            write_token: Some("a_valid_write_token_5678".to_string()),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_only_write_token() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("a_valid_write_token_5678".to_string()),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_trusted_proxies_accepts_ip_and_cidr() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("a_valid_write_token_5678".to_string()),
            trusted_proxies: vec!["10.0.0.0/8".to_string(), "192.168.1.10".to_string()],
            ..Default::default()
        };
        assert!(config.validate().is_ok());
        let nets = config.trusted_proxy_nets().unwrap();
        assert_eq!(nets.len(), 2);
    }

    #[test]
    fn test_validate_trusted_proxies_rejects_invalid_entry() {
        let config = AuthConfig {
            enabled: true,
            write_token: Some("a_valid_write_token_5678".to_string()),
            trusted_proxies: vec!["not-a-cidr".to_string()],
            ..Default::default()
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("auth.trusted_proxies"));
    }

    #[test]
    fn test_generate_token_length() {
        let token = AuthConfig::generate_token();
        // 32 bytes = 64 hex characters
        assert_eq!(token.len(), 64);
    }

    #[test]
    fn test_generate_token_is_hex() {
        let token = AuthConfig::generate_token();
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_generate_token_unique() {
        let a = AuthConfig::generate_token();
        let b = AuthConfig::generate_token();
        assert_ne!(a, b);
    }

    #[test]
    fn test_generate_default() {
        let config = AuthConfig::generate_default();
        assert!(config.enabled);
        assert!(config.read_token.is_some());
        assert!(config.write_token.is_some());
        assert!(config.require_tls);
        assert_eq!(config.rate_limit_per_second, 10);
        assert_eq!(config.rate_limit_burst, 20);
        assert!(config.trusted_proxies.is_empty());
        assert_ne!(config.read_token, config.write_token);
    }

    #[test]
    fn test_from_legacy_token() {
        let config = AuthConfig::from_legacy_token("my-admin-token-1234567890");
        assert!(config.enabled);
        assert!(config.read_token.is_none());
        assert_eq!(
            config.write_token,
            Some("my-admin-token-1234567890".to_string())
        );
        assert!(!config.require_tls);
        assert!(config.trusted_proxies.is_empty());
    }

    #[test]
    fn test_token_type_display() {
        assert_eq!(format!("{}", TokenType::None), "none");
        assert_eq!(format!("{}", TokenType::ReadToken), "read_token");
        assert_eq!(format!("{}", TokenType::WriteToken), "write_token");
    }
}

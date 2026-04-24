use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::GatewayError;

pub use common::types::OidcConfig;

/// Cached JWKS with the last fetch time.
struct JwksCache {
    /// Map from key ID (kid) to the decoded public key.
    keys: HashMap<String, jsonwebtoken::DecodingKey>,

    /// When the JWKS was last fetched.
    fetched_at: std::time::Instant,
}

/// Cached OIDC provider state.
pub struct OidcProvider {
    config: OidcConfig,

    /// The JWKS (JSON Web Key Set) used to verify JWT signatures.
    /// Fetched from <issuer_url>/protocol/openid-connect/certs
    /// and refreshed periodically.
    jwks: Arc<RwLock<JwksCache>>,

    /// HTTP client for fetching JWKS and OIDC discovery.
    http_client: reqwest::Client,
}

impl OidcProvider {
    pub fn new(config: OidcConfig) -> Self {
        OidcProvider {
            config,
            jwks: Arc::new(RwLock::new(JwksCache {
                keys: HashMap::new(),
                fetched_at: std::time::Instant::now()
                    - std::time::Duration::from_secs(3601), // force initial fetch
            })),
            http_client: reqwest::Client::new(),
        }
    }

    /// Start the background JWKS refresh loop.
    pub fn start_refresh_loop(self: Arc<Self>) {
        let provider = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                provider.config.jwks_refresh_secs,
            ));
            loop {
                interval.tick().await;
                if let Err(e) = provider.refresh_jwks().await {
                    tracing::error!(error = %e, "failed to refresh JWKS");
                }
            }
        });
    }

    /// Fetch the JWKS from the OIDC provider.
    pub async fn refresh_jwks(&self) -> Result<(), GatewayError> {
        let jwks_url = format!(
            "{}/protocol/openid-connect/certs",
            self.config.issuer_url.trim_end_matches('/')
        );

        let resp = self
            .http_client
            .get(&jwks_url)
            .send()
            .await
            .map_err(|e| GatewayError::Oidc(format!("JWKS fetch failed: {e}")))?;

        let jwks_json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| GatewayError::Oidc(format!("JWKS parse failed: {e}")))?;

        let mut keys = HashMap::new();
        if let Some(key_array) = jwks_json.get("keys").and_then(|k| k.as_array()) {
            for key_json in key_array {
                if let (Some(kid), Some(kty), Some(n), Some(e)) = (
                    key_json.get("kid").and_then(|v| v.as_str()),
                    key_json.get("kty").and_then(|v| v.as_str()),
                    key_json.get("n").and_then(|v| v.as_str()),
                    key_json.get("e").and_then(|v| v.as_str()),
                ) {
                    if kty == "RSA" {
                        if let Ok(decoding_key) = jsonwebtoken::DecodingKey::from_rsa_components(n, e) {
                            keys.insert(kid.to_string(), decoding_key);
                        }
                    }
                }
            }
        }

        let mut cache = self.jwks.write().await;
        let key_count = keys.len();
        cache.keys = keys;
        cache.fetched_at = std::time::Instant::now();
        tracing::info!(key_count, "JWKS refreshed from OIDC provider");

        Ok(())
    }

    /// Validate a JWT token and extract the user identity.
    pub async fn validate_token(&self, token: &str) -> Result<UserIdentity, GatewayError> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| GatewayError::Auth(format!("invalid JWT header: {e}")))?;
        let kid = header
            .kid
            .ok_or_else(|| GatewayError::Auth("JWT missing kid header".to_string()))?;

        let cache = self.jwks.read().await;

        if cache.fetched_at.elapsed().as_secs() > self.config.jwks_refresh_secs * 2 {
            drop(cache);
            self.refresh_jwks().await?;
            let cache = self.jwks.read().await;
            return self.validate_with_cache(token, kid, &cache).await;
        }

        self.validate_with_cache(token, kid, &cache).await
    }

    async fn validate_with_cache(
        &self,
        token: &str,
        kid: String,
        cache: &JwksCache,
    ) -> Result<UserIdentity, GatewayError> {
        let decoding_key = cache
            .keys
            .get(&kid)
            .ok_or_else(|| GatewayError::Auth(format!("unknown key ID: {}", kid)))?;

        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_audience(&[&self.config.audience]);
        validation.set_issuer(&[&self.config.issuer_url]);
        validation.leeway = self.config.clock_skew_secs;

        let token_data = jsonwebtoken::decode::<serde_json::Value>(token, decoding_key, &validation)
            .map_err(|e| GatewayError::Auth(format!("JWT validation failed: {e}")))?;

        let claims = &token_data.claims;
        let sub = claims
            .get("sub")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let email = claims
            .get("email")
            .and_then(|v| v.as_str())
            .map(String::from);

        let realm_roles = claims
            .get("realm_access")
            .and_then(|ra| ra.get("roles"))
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let client_roles = claims
            .get("resource_access")
            .and_then(|ra| ra.as_object())
            .map(|obj| {
                obj.iter()
                    .flat_map(|(client_id, client_val)| {
                        client_val
                            .get("roles")
                            .and_then(|r| r.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default()
                            .into_iter()
                            .map(move |role| format!("{}:{}", client_id, role))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let all_roles: Vec<String> = realm_roles.into_iter().chain(client_roles).collect();

        Ok(UserIdentity {
            sub,
            email,
            roles: all_roles,
            raw_claims: token_data.claims.clone(),
        })
    }
}

impl OidcProvider {
    /// Inject a JWKS key directly into the cache (test helper).
    pub async fn inject_jwks_key(&self, kid: String, key: jsonwebtoken::DecodingKey) {
        let mut cache = self.jwks.write().await;
        cache.keys.insert(kid, key);
        cache.fetched_at = std::time::Instant::now();
    }

    /// Set the cache fetch time (test helper).
    pub async fn set_cache_fetched_at(&self, instant: std::time::Instant) {
        let mut cache = self.jwks.write().await;
        cache.fetched_at = instant;
    }
}

/// Extracted user identity from a validated JWT.
#[derive(Debug, Clone)]
pub struct UserIdentity {
    /// Subject — unique user identifier (Keycloak user ID).
    pub sub: String,

    /// Email (if present in token).
    pub email: Option<String>,

    /// All roles (realm + client-scoped).
    pub roles: Vec<String>,

    /// Raw JWT claims for custom extraction.
    pub raw_claims: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oidc_config_defaults() {
        let config = OidcConfig {
            issuer_url: "https://keycloak.example.com/realms/test".to_string(),
            audience: "my-app".to_string(),
            jwks_refresh_secs: 3600,
            clock_skew_secs: 30,
        };
        assert_eq!(config.jwks_refresh_secs, 3600);
        assert_eq!(config.clock_skew_secs, 30);
    }

    #[test]
    fn test_oidc_config_serialization() {
        let config = OidcConfig {
            issuer_url: "https://keycloak.example.com/realms/test".to_string(),
            audience: "my-app".to_string(),
            jwks_refresh_secs: 1800,
            clock_skew_secs: 60,
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("https://keycloak.example.com/realms/test"));
        assert!(json.contains("my-app"));

        let decoded: OidcConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.jwks_refresh_secs, 1800);
        assert_eq!(decoded.clock_skew_secs, 60);
    }
}

use common::auth::AuthConfig;
use serde::Deserialize;

/// Request body for token rotation endpoint.
#[derive(Debug, Deserialize)]
pub struct RotateTokenRequest {
    /// Which token to rotate: "read" or "write".
    pub token_type: String,

    /// The new token value. If not provided, a random token is generated.
    pub new_token: Option<String>,
}

/// Validate a token rotation request and return the new token value.
///
/// Returns `Ok(new_token)` if the request is valid, or an error message.
pub fn validate_rotation_request(req: &RotateTokenRequest) -> Result<String, String> {
    if req.token_type != "read" && req.token_type != "write" {
        return Err("token_type must be 'read' or 'write'".to_string());
    }

    let new_token = req
        .new_token
        .clone()
        .unwrap_or_else(AuthConfig::generate_token);

    if new_token.len() < 16 {
        return Err(format!(
            "new token must be at least 16 characters (got {})",
            new_token.len()
        ));
    }

    Ok(new_token)
}

/// Check if a config file has overly permissive permissions.
///
/// On Unix, warns if the file is readable by group or others (mode & 0o077 != 0).
/// This prevents accidental exposure of auth tokens in shared environments.
pub fn check_config_file_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mode = metadata.permissions().mode();
            if mode & 0o077 != 0 {
                tracing::warn!(
                    path = %path.display(),
                    mode = format!("{:o}", mode & 0o777),
                    "config file has overly permissive permissions - \
                     other users can read the auth tokens. \
                     Recommended: chmod 600"
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Check if the admin API TLS requirement is satisfied at startup.
///
/// Returns an error if auth is enabled with `require_tls = true` but
/// no TLS certificate is configured for the admin API.
pub fn check_admin_tls_requirement(
    auth_config: &AuthConfig,
    admin_tls_configured: bool,
) -> Result<(), String> {
    if !auth_config.enabled {
        return Ok(());
    }

    if !auth_config.require_tls {
        tracing::warn!(
            "Admin API authentication is enabled but TLS is NOT required. \
             Bearer tokens will be sent over plaintext HTTP. \
             Set auth.require_tls = true in production."
        );
        return Ok(());
    }

    if !admin_tls_configured {
        return Err(
            "Admin API requires TLS when authentication is enabled, \
             but no TLS certificate is configured. \
             Either:\n\
             1. Configure admin.tls_cert / admin.tls_key (or shared proxy.tls_cert / proxy.tls_key) for the admin HTTPS listener\n\
             2. Set auth.require_tls = false (NOT recommended for production)\n\
             3. Disable authentication (auth.enabled = false, NOT recommended)"
                .to_string(),
        );
    }

    Ok(())
}

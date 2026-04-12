use std::path::Path;

/// Load TLS cert and key from PEM files.
/// In production, use certbot + ACME or mount from secrets.
pub fn tls_settings(cert_pem: &Path, key_pem: &Path) -> (String, String) {
    (
        cert_pem.to_str().unwrap().to_string(),
        key_pem.to_str().unwrap().to_string(),
    )
}

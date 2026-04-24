use super::config::{CorsPolicy, GatewayRouteConfig};
use pingora_core::Result as PingoraResult;
use pingora_proxy::Session;

/// Handle a CORS preflight (OPTIONS) request.
/// Returns true to abort the request (we send the response ourselves).
pub async fn handle_cors_preflight(
    session: &mut Session,
    route_config: &GatewayRouteConfig,
) -> PingoraResult<bool> {
    let cors = match &route_config.cors {
        Some(c) => c,
        None => return Ok(false), // no CORS config, pass through
    };

    let origin = session
        .req_header()
        .headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Check if the origin is allowed
    if !is_origin_allowed(origin, cors) {
        session.respond_error(403).await?;
        return Ok(true);
    }

    // Build CORS response headers
    let mut resp = pingora::http::ResponseHeader::build(200, None)
        .map_err(|e| pingora_core::Error::because(
            pingora_core::ErrorType::InternalError,
            "CORS response build",
            e,
        ))?;
    let _ = resp.insert_header("Access-Control-Allow-Origin", origin);
    let _ = resp.insert_header(
        "Access-Control-Allow-Methods",
        cors.allowed_methods.join(", "),
    );
    let _ = resp.insert_header(
        "Access-Control-Allow-Headers",
        cors.allowed_headers.join(", "),
    );
    let _ = resp.insert_header("Access-Control-Max-Age", cors.max_age_secs.to_string());

    if cors.allow_credentials {
        let _ = resp.insert_header("Access-Control-Allow-Credentials", "true");
    }

    if !cors.expose_headers.is_empty() {
        let _ = resp.insert_header(
            "Access-Control-Expose-Headers",
            cors.expose_headers.join(", "),
        );
    }

    // Send the preflight response
    session.write_response_header(Box::new(resp), true).await?;

    Ok(true) // abort — we've sent the response
}

/// Add CORS headers to a normal (non-preflight) response.
pub fn add_cors_headers(
    upstream_response: &mut pingora::http::ResponseHeader,
    route_config: &GatewayRouteConfig,
    origin: &str,
) {
    let cors = match &route_config.cors {
        Some(c) => c,
        None => return,
    };

    if !is_origin_allowed(origin, cors) {
        return;
    }

    let _ = upstream_response.insert_header("Access-Control-Allow-Origin", origin);
    let _ = upstream_response.insert_header(
        "Access-Control-Allow-Methods",
        cors.allowed_methods.join(", "),
    );
    let _ = upstream_response.insert_header(
        "Access-Control-Allow-Headers",
        cors.allowed_headers.join(", "),
    );

    if cors.allow_credentials {
        let _ = upstream_response.insert_header("Access-Control-Allow-Credentials", "true");
    }

    if !cors.expose_headers.is_empty() {
        let _ = upstream_response.insert_header(
            "Access-Control-Expose-Headers",
            cors.expose_headers.join(", "),
        );
    }
}

pub fn is_origin_allowed(origin: &str, cors: &CorsPolicy) -> bool {
    if cors.allowed_origins.contains(&"*".to_string()) {
        return true;
    }
    cors.allowed_origins.iter().any(|allowed| {
        allowed == origin
            || (allowed.starts_with("*.") && origin.ends_with(&allowed[1..]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::types::CorsPolicy;

    #[test]
    fn test_is_origin_allowed_wildcard() {
        let cors = CorsPolicy {
            allowed_origins: vec!["*".to_string()],
            allowed_methods: CorsPolicy::default_methods(),
            allowed_headers: CorsPolicy::default_headers(),
            expose_headers: vec![],
            allow_credentials: false,
            max_age_secs: 86400,
        };
        assert!(is_origin_allowed("https://any.com", &cors));
        assert!(is_origin_allowed("http://evil.com", &cors));
    }

    #[test]
    fn test_is_origin_allowed_subdomain() {
        let cors = CorsPolicy {
            allowed_origins: vec!["*.example.com".to_string()],
            allowed_methods: CorsPolicy::default_methods(),
            allowed_headers: CorsPolicy::default_headers(),
            expose_headers: vec![],
            allow_credentials: false,
            max_age_secs: 86400,
        };
        assert!(is_origin_allowed("https://app.example.com", &cors));
        assert!(is_origin_allowed("https://api.example.com", &cors));
        assert!(!is_origin_allowed("https://example.com", &cors));
        assert!(!is_origin_allowed("https://evil.com", &cors));
    }

    #[test]
    fn test_is_origin_allowed_exact() {
        let cors = CorsPolicy {
            allowed_origins: vec!["https://app.example.com".to_string()],
            allowed_methods: CorsPolicy::default_methods(),
            allowed_headers: CorsPolicy::default_headers(),
            expose_headers: vec![],
            allow_credentials: false,
            max_age_secs: 86400,
        };
        assert!(is_origin_allowed("https://app.example.com", &cors));
        assert!(!is_origin_allowed("https://api.example.com", &cors));
    }
}

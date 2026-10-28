use super::config::RequestTransform;
use super::oidc::UserIdentity;
use pingora::http::RequestHeader;

/// Apply request transformations before forwarding to upstream.
pub fn apply_request_transform(
    request: &mut RequestHeader,
    transform: &RequestTransform,
    user_identity: Option<&UserIdentity>,
) {
    // Add custom headers from route config
    for (key, value) in &transform.add_headers {
        if super::identity::is_identity_header(key) {
            continue;
        }
        if let Ok(name) = http::header::HeaderName::from_bytes(key.as_bytes()) {
            let _ = request.headers.insert(
                name,
                http::header::HeaderValue::from_str(value)
                    .unwrap_or_else(|_| http::header::HeaderValue::from_static("")),
            );
        }
    }

    // Remove headers from route config
    for key in &transform.remove_headers {
        if let Ok(name) = http::header::HeaderName::from_bytes(key.as_bytes()) {
            request.headers.remove(name);
        }
    }

    // Path prefix injection
    if let Some(ref prefix) = transform.path_prefix {
        let original = request.uri.to_string();
        let new_path = format!("{}{}", prefix.trim_end_matches('/'), original);
        if let Ok(parsed) = new_path.parse() {
            request.uri = parsed;
        }
    }

    // Strip query parameters
    if !transform.strip_query_params.is_empty() {
        let original = request.uri.to_string();
        if let Some((path, query)) = original.split_once('?') {
            let remaining: Vec<&str> = query
                .split('&')
                .filter(|pair| {
                    let key = pair.split('=').next().unwrap_or("");
                    !transform.strip_query_params.iter().any(|s| s == key)
                })
                .collect();
            let new_uri = if remaining.is_empty() {
                path.to_string()
            } else {
                format!("{}?{}", path, remaining.join("&"))
            };
            if let Ok(parsed) = new_uri.parse() {
                request.uri = parsed;
            }
        }
    }
    super::identity::write_user_identity(&mut request.headers, user_identity);
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::types::RequestTransform;

    #[test]
    fn test_request_transform_add_headers() {
        let mut req = RequestHeader::build(http::Method::GET, b"/", None).unwrap();
        let transform = RequestTransform {
            add_headers: vec![("X-Api-Version".to_string(), "2".to_string())],
            ..Default::default()
        };
        apply_request_transform(&mut req, &transform, None);
        assert_eq!(
            req.headers.get("X-Api-Version").unwrap().to_str().unwrap(),
            "2"
        );
    }

    #[test]
    fn test_request_transform_remove_headers() {
        let mut req = RequestHeader::build(http::Method::GET, b"/", None).unwrap();
        req.headers.insert(
            "X-Internal-Token",
            http::header::HeaderValue::from_static("secret"),
        );
        let transform = RequestTransform {
            remove_headers: vec!["X-Internal-Token".to_string()],
            ..Default::default()
        };
        apply_request_transform(&mut req, &transform, None);
        assert!(req.headers.get("X-Internal-Token").is_none());
    }

    #[test]
    fn test_request_transform_path_prefix() {
        let mut req = RequestHeader::build(http::Method::GET, b"/users", None).unwrap();
        let transform = RequestTransform {
            path_prefix: Some("/api/v2".to_string()),
            ..Default::default()
        };
        apply_request_transform(&mut req, &transform, None);
        assert_eq!(req.uri.path(), "/api/v2/users");
    }

    #[test]
    fn test_request_transform_strip_query() {
        let mut req =
            RequestHeader::build(http::Method::GET, b"/search?q=rust&tracking=123", None).unwrap();
        let transform = RequestTransform {
            strip_query_params: vec!["tracking".to_string()],
            ..Default::default()
        };
        apply_request_transform(&mut req, &transform, None);
        assert_eq!(req.uri.query(), Some("q=rust"));
    }

    #[test]
    fn test_user_identity_headers() {
        let mut req = RequestHeader::build(http::Method::GET, b"/", None).unwrap();
        let identity = UserIdentity {
            sub: "user-123".to_string(),
            email: Some("test@example.com".to_string()),
            roles: vec!["admin".to_string(), "user".to_string()],
            raw_claims: serde_json::json!({}),
        };
        apply_request_transform(&mut req, &RequestTransform::default(), Some(&identity));
        assert_eq!(
            req.headers.get("X-User-Id").unwrap().to_str().unwrap(),
            "user-123"
        );
        assert_eq!(
            req.headers.get("X-User-Email").unwrap().to_str().unwrap(),
            "test@example.com"
        );
        assert_eq!(
            req.headers.get("X-User-Roles").unwrap().to_str().unwrap(),
            "admin,user"
        );
    }

    #[test]
    fn route_transform_cannot_override_authenticated_identity() {
        let mut req = RequestHeader::build(http::Method::GET, b"/", None).unwrap();
        let transform = RequestTransform {
            add_headers: vec![
                ("X-User-Roles".into(), "admin".into()),
                ("X-Source-App".into(), "forged-app".into()),
            ],
            remove_headers: vec!["X-User-Id".into()],
            ..Default::default()
        };
        let identity = UserIdentity {
            sub: "verified-user".into(),
            email: None,
            roles: vec![],
            raw_claims: serde_json::json!({}),
        };
        apply_request_transform(&mut req, &transform, Some(&identity));
        assert_eq!(req.headers["x-user-id"], "verified-user");
        assert!(!req.headers.contains_key("x-user-roles"));
        assert!(!req.headers.contains_key("x-source-app"));
    }
}

//! Authoritative HTTP identity shared by the edge and internal gateways.

use http::{HeaderMap, HeaderName, HeaderValue};

use super::oidc::UserIdentity;

/// Reserved identity headers must never be inherited from an HTTP caller.
pub fn is_identity_header(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("x-user-")
        || name.to_ascii_lowercase().starts_with("x-trigger-")
        || [
            "x-app-id",
            "x-namespace",
            "x-source-app",
            "x-source-namespace",
            "x-source-tid",
        ]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// Remove all values, including duplicates and future `X-User-*` extensions.
pub fn strip_identity_headers(headers: &mut HeaderMap) {
    let names: Vec<HeaderName> = headers
        .keys()
        .filter(|name| is_identity_header(name.as_str()))
        .cloned()
        .collect();
    for name in names {
        headers.remove(name);
    }
}

/// Replace user claims with values from gateway authentication only.
pub fn write_user_identity(headers: &mut HeaderMap, identity: Option<&UserIdentity>) {
    let names: Vec<HeaderName> = headers
        .keys()
        .filter(|name| name.as_str().starts_with("x-user-"))
        .cloned()
        .collect();
    for name in names {
        headers.remove(name);
    }
    let Some(identity) = identity else {
        return;
    };
    if let Ok(value) = HeaderValue::from_str(&identity.sub) {
        headers.insert("x-user-id", value);
    }
    if let Some(email) = &identity.email {
        if let Ok(value) = HeaderValue::from_str(email) {
            headers.insert("x-user-email", value);
        }
    }
    if !identity.roles.is_empty() {
        if let Ok(value) = HeaderValue::from_str(&identity.roles.join(",")) {
            headers.insert("x-user-roles", value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateways_strip_all_trigger_identity_values() {
        let mut headers = HeaderMap::new();
        for name in ["x-trigger-id", "x-trigger-node", "x-trigger-future"] {
            headers.append(name, HeaderValue::from_static("forged"));
            headers.append(name, HeaderValue::from_static("duplicate"));
        }
        headers.insert("content-type", HeaderValue::from_static("text/plain"));
        strip_identity_headers(&mut headers);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["content-type"], "text/plain");
    }

    #[test]
    fn anonymous_and_partial_identity_remove_all_forged_user_claims() {
        for authenticated in [false, true] {
            let mut headers = HeaderMap::new();
            for name in ["X-User-Id", "X-User-Email", "X-User-Roles", "X-User-Tenant"] {
                let name = HeaderName::from_bytes(name.as_bytes()).unwrap();
                headers.append(name.clone(), HeaderValue::from_static("forged"));
                headers.append(name, HeaderValue::from_static("duplicate"));
            }
            let identity = UserIdentity {
                sub: "verified-user".into(),
                email: None,
                roles: vec![],
                raw_claims: serde_json::json!({}),
            };
            write_user_identity(&mut headers, authenticated.then_some(&identity));
            assert_eq!(
                headers.get_all("x-user-id").iter().count(),
                usize::from(authenticated)
            );
            if authenticated {
                assert_eq!(headers["x-user-id"], "verified-user");
            }
            for name in ["x-user-email", "x-user-roles", "x-user-tenant"] {
                assert!(!headers.contains_key(name));
            }
        }
    }
}

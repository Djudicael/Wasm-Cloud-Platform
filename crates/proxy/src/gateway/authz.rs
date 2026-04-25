use super::config::AuthPolicy;
use super::oidc::UserIdentity;

/// Check if a user identity satisfies the route's auth policy.
pub fn authorize(identity: &UserIdentity, policy: &AuthPolicy) -> bool {
    match policy {
        AuthPolicy::None => true,
        AuthPolicy::Authenticated => true, // token already validated
        AuthPolicy::Roles {
            allowed_roles,
            client_id,
        } => authorize_roles(identity, allowed_roles, client_id.as_deref()),
    }
}

/// Check if a user has any of the allowed roles.
pub fn authorize_roles(
    identity: &UserIdentity,
    allowed_roles: &[String],
    client_id: Option<&str>,
) -> bool {
    allowed_roles.iter().any(|required| {
        identity.roles.iter().any(|r| r == required)
            || client_id.map_or(false, |cid| {
                identity.roles.iter().any(|r| r == &format!("{}:{}", cid, required))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_identity(roles: Vec<String>) -> UserIdentity {
        UserIdentity {
            sub: "user-123".to_string(),
            email: Some("test@example.com".to_string()),
            roles,
            raw_claims: serde_json::json!({}),
        }
    }

    #[test]
    fn test_auth_policy_none_allows_all() {
        let identity = test_identity(vec!["admin".to_string()]);
        assert!(authorize(&identity, &AuthPolicy::None));
    }

    #[test]
    fn test_auth_policy_authenticated_allows_any_valid_user() {
        let identity = test_identity(vec![]);
        assert!(authorize(&identity, &AuthPolicy::Authenticated));
    }

    #[test]
    fn test_auth_policy_roles_matching_realm_role() {
        let identity = test_identity(vec!["admin".to_string(), "user".to_string()]);
        let policy = AuthPolicy::Roles {
            allowed_roles: vec!["admin".to_string()],
            client_id: None,
        };
        assert!(authorize(&identity, &policy));
    }

    #[test]
    fn test_auth_policy_roles_no_match() {
        let identity = test_identity(vec!["user".to_string()]);
        let policy = AuthPolicy::Roles {
            allowed_roles: vec!["admin".to_string()],
            client_id: None,
        };
        assert!(!authorize(&identity, &policy));
    }

    #[test]
    fn test_auth_policy_roles_matching_client_role() {
        let identity = test_identity(vec!["my-api:admin".to_string()]);
        let policy = AuthPolicy::Roles {
            allowed_roles: vec!["admin".to_string()],
            client_id: Some("my-api".to_string()),
        };
        assert!(authorize(&identity, &policy));
    }

    #[test]
    fn test_auth_policy_roles_or_logic() {
        let identity = test_identity(vec!["editor".to_string()]);
        let policy = AuthPolicy::Roles {
            allowed_roles: vec!["admin".to_string(), "editor".to_string()],
            client_id: None,
        };
        assert!(authorize(&identity, &policy));
    }
}

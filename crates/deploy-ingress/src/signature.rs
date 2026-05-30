use crate::{app::SignaturePolicy, audit::now_unix_secs};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use common::{
    deploy::{ArtifactSignature, ArtifactVerificationRecord, RemoteArtifactSource},
    error::PlatformError,
};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::Digest;
use sigstore_verify::{
    trust_root::{TrustedRoot, SIGSTORE_PRODUCTION_TRUSTED_ROOT},
    types::{Bundle, Sha256Hash},
    VerificationPolicy, Verifier as SigstoreVerifier,
};

pub fn verify_artifact_signature(
    policy: &SignaturePolicy,
    artifact_sha256: &str,
    artifact: &RemoteArtifactSource,
) -> Result<ArtifactVerificationRecord, PlatformError> {
    let signature = artifact.signature.as_ref();
    if policy.require_signature && signature.is_none() {
        return Err(PlatformError::security(
            "artifact signature is required by deploy-ingress policy",
        ));
    }

    let Some(signature_meta) = signature else {
        return Ok(ArtifactVerificationRecord {
            sha256: artifact_sha256.to_string(),
            verified: false,
            algorithm: None,
            issuer: None,
            identity: None,
            repository: None,
            namespace: None,
            public_key_sha256: None,
            verified_at_unix_secs: now_unix_secs(),
        });
    };

    let public_key_bytes = STANDARD.decode(&signature_meta.public_key).map_err(|e| {
        PlatformError::security(format!(
            "invalid artifact signature public key encoding: {e}"
        ))
    })?;
    let signature_bytes = STANDARD.decode(&signature_meta.signature).map_err(|e| {
        PlatformError::security(format!("invalid artifact signature encoding: {e}"))
    })?;

    let public_key_sha256 = hex::encode(sha2::Sha256::digest(&public_key_bytes));
    let public_key: [u8; 32] = public_key_bytes
        .try_into()
        .map_err(|_| PlatformError::security("artifact signature public key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&public_key)
        .map_err(|e| PlatformError::security(format!("invalid Ed25519 public key: {e}")))?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|e| PlatformError::security(format!("invalid Ed25519 signature: {e}")))?;

    match signature_meta.algorithm.to_lowercase().as_str() {
        "ed25519" => verify_native_signature(
            policy,
            artifact_sha256,
            signature_meta,
            &verifying_key,
            &signature,
            public_key_sha256,
        ),
        "cosign-ed25519" => verify_cosign_payload_signature(
            policy,
            artifact_sha256,
            signature_meta,
            &verifying_key,
            &signature,
            public_key_sha256,
        ),
        "sigstore-bundle" => verify_sigstore_bundle_signature(
            policy,
            artifact_sha256,
            signature_meta,
            public_key_sha256,
        ),
        other => Err(PlatformError::security(format!(
            "unsupported artifact signature algorithm: {}",
            other
        ))),
    }
}

fn verify_native_signature(
    policy: &SignaturePolicy,
    artifact_sha256: &str,
    signature_meta: &ArtifactSignature,
    verifying_key: &VerifyingKey,
    signature: &Signature,
    public_key_sha256: String,
) -> Result<ArtifactVerificationRecord, PlatformError> {
    enforce_allowed_claim(
        "issuer",
        signature_meta.issuer.as_deref(),
        &policy.allowed_issuers,
    )?;
    enforce_allowed_claim(
        "identity",
        signature_meta.identity.as_deref(),
        &policy.allowed_identities,
    )?;
    enforce_allowed_claim(
        "repository",
        signature_meta.repository.as_deref(),
        &policy.allowed_repositories,
    )?;
    enforce_allowed_claim(
        "namespace",
        signature_meta.namespace.as_deref(),
        &policy.allowed_namespaces,
    )?;

    let claims = serde_json::to_vec(&serde_json::json!({
        "sha256": artifact_sha256,
        "issuer": signature_meta.issuer,
        "identity": signature_meta.identity,
        "repository": signature_meta.repository,
        "namespace": signature_meta.namespace,
    }))
    .map_err(|e| {
        PlatformError::internal(format!(
            "failed to serialize artifact signature claims: {e}"
        ))
    })?;

    verifying_key.verify(&claims, signature).map_err(|e| {
        PlatformError::security(format!("artifact signature verification failed: {e}"))
    })?;

    Ok(ArtifactVerificationRecord {
        sha256: artifact_sha256.to_string(),
        verified: true,
        algorithm: Some("ed25519".to_string()),
        issuer: signature_meta.issuer.clone(),
        identity: signature_meta.identity.clone(),
        repository: signature_meta.repository.clone(),
        namespace: signature_meta.namespace.clone(),
        public_key_sha256: Some(public_key_sha256),
        verified_at_unix_secs: now_unix_secs(),
    })
}

fn verify_cosign_payload_signature(
    policy: &SignaturePolicy,
    artifact_sha256: &str,
    signature_meta: &ArtifactSignature,
    verifying_key: &VerifyingKey,
    signature: &Signature,
    public_key_sha256: String,
) -> Result<ArtifactVerificationRecord, PlatformError> {
    let payload = signature_meta.payload.as_deref().ok_or_else(|| {
        PlatformError::security("cosign-ed25519 signatures require an inline payload")
    })?;
    verifying_key
        .verify(payload.as_bytes(), signature)
        .map_err(|e| {
            PlatformError::security(format!("artifact signature verification failed: {e}"))
        })?;

    let payload_json: serde_json::Value = serde_json::from_str(payload)
        .map_err(|e| PlatformError::security(format!("cosign payload is not valid json: {e}")))?;

    let digest = payload_json
        .get("critical")
        .and_then(|v| v.get("image"))
        .and_then(|v| v.get("docker-manifest-digest"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            PlatformError::security(
                "cosign payload is missing critical.image.docker-manifest-digest",
            )
        })?;
    if digest != format!("sha256:{artifact_sha256}") {
        return Err(PlatformError::security(format!(
            "cosign payload digest mismatch: expected sha256:{artifact_sha256}, got {digest}"
        )));
    }

    let repository = payload_json
        .get("optional")
        .and_then(|v| v.get("repository"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            payload_json
                .get("critical")
                .and_then(|v| v.get("identity"))
                .and_then(|v| v.get("docker-reference"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let issuer = payload_json
        .get("optional")
        .and_then(|v| v.get("issuer"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| signature_meta.issuer.clone());
    let identity = payload_json
        .get("optional")
        .and_then(|v| v.get("identity"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| signature_meta.identity.clone());
    let namespace = payload_json
        .get("optional")
        .and_then(|v| v.get("namespace"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| signature_meta.namespace.clone());

    enforce_allowed_claim("issuer", issuer.as_deref(), &policy.allowed_issuers)?;
    enforce_allowed_claim("identity", identity.as_deref(), &policy.allowed_identities)?;
    enforce_allowed_claim(
        "repository",
        repository.as_deref(),
        &policy.allowed_repositories,
    )?;
    enforce_allowed_claim(
        "namespace",
        namespace.as_deref(),
        &policy.allowed_namespaces,
    )?;

    Ok(ArtifactVerificationRecord {
        sha256: artifact_sha256.to_string(),
        verified: true,
        algorithm: Some("cosign-ed25519".to_string()),
        issuer,
        identity,
        repository,
        namespace,
        public_key_sha256: Some(public_key_sha256),
        verified_at_unix_secs: now_unix_secs(),
    })
}

// Sigstore bundle verification currently binds the deploy policy to issuer and identity.
// Repository and namespace matching can be layered later once the platform stores and
// validates richer bundle claims consistently across OCI registries.
fn verify_sigstore_bundle_signature(
    policy: &SignaturePolicy,
    artifact_sha256: &str,
    signature_meta: &ArtifactSignature,
    public_key_sha256: String,
) -> Result<ArtifactVerificationRecord, PlatformError> {
    if !policy.allowed_repositories.is_empty() || !policy.allowed_namespaces.is_empty() {
        return Err(PlatformError::security(
            "sigstore-bundle verification currently supports issuer/identity policy only",
        ));
    }

    let payload = signature_meta.payload.as_deref().ok_or_else(|| {
        PlatformError::security("sigstore-bundle signatures require an inline bundle payload")
    })?;
    let issuer = signature_meta.issuer.as_deref().ok_or_else(|| {
        PlatformError::security("sigstore-bundle signatures require an issuer claim")
    })?;
    let identity = signature_meta.identity.as_deref().ok_or_else(|| {
        PlatformError::security("sigstore-bundle signatures require an identity claim")
    })?;

    enforce_allowed_claim("issuer", Some(issuer), &policy.allowed_issuers)?;
    enforce_allowed_claim("identity", Some(identity), &policy.allowed_identities)?;

    let trusted_root = TrustedRoot::from_json(SIGSTORE_PRODUCTION_TRUSTED_ROOT).map_err(|e| {
        PlatformError::security(format!("failed to load Sigstore trusted root: {e}"))
    })?;
    let bundle = Bundle::from_json(payload)
        .map_err(|e| PlatformError::security(format!("invalid Sigstore bundle: {e}")))?;
    let digest = Sha256Hash::from_hex(artifact_sha256).map_err(|e| {
        PlatformError::security(format!(
            "invalid artifact digest for Sigstore verification: {e}"
        ))
    })?;

    let verification_policy = VerificationPolicy::default()
        .require_identity(identity)
        .require_issuer(issuer);
    let verifier = SigstoreVerifier::new(&trusted_root);
    verifier
        .verify(digest, &bundle, &verification_policy)
        .map_err(|e| {
            PlatformError::security(format!("Sigstore bundle verification failed: {e}"))
        })?;

    Ok(ArtifactVerificationRecord {
        sha256: artifact_sha256.to_string(),
        verified: true,
        algorithm: Some("sigstore-bundle".to_string()),
        issuer: Some(issuer.to_string()),
        identity: Some(identity.to_string()),
        repository: None,
        namespace: None,
        public_key_sha256: Some(public_key_sha256),
        verified_at_unix_secs: now_unix_secs(),
    })
}

fn enforce_allowed_claim(
    claim_name: &str,
    value: Option<&str>,
    allowed: &[String],
) -> Result<(), PlatformError> {
    if allowed.is_empty() {
        return Ok(());
    }
    let Some(value) = value else {
        return Err(PlatformError::security(format!(
            "artifact signature is missing required {} claim",
            claim_name
        )));
    };
    if allowed.iter().any(|candidate| candidate == value) {
        return Ok(());
    }
    Err(PlatformError::security(format!(
        "artifact signature {} claim '{}' is not allowed by deploy-ingress policy",
        claim_name, value
    )))
}

#[cfg(test)]
mod tests {
    use super::verify_artifact_signature;
    use crate::app::SignaturePolicy;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use common::deploy::{ArtifactSignature, RemoteArtifactSource};
    use ed25519_dalek::{Signer, SigningKey};

    fn sign_artifact_claims(
        sha256: &str,
        issuer: Option<&str>,
        repository: Option<&str>,
        namespace: Option<&str>,
    ) -> ArtifactSignature {
        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let claims = serde_json::to_vec(&serde_json::json!({
            "sha256": sha256,
            "issuer": issuer,
            "identity": serde_json::Value::Null,
            "repository": repository,
            "namespace": namespace,
        }))
        .unwrap();
        let signature = signing_key.sign(&claims);
        ArtifactSignature {
            algorithm: "ed25519".to_string(),
            public_key: STANDARD.encode(signing_key.verifying_key().to_bytes()),
            signature: STANDARD.encode(signature.to_bytes()),
            payload: None,
            issuer: issuer.map(ToOwned::to_owned),
            identity: None,
            repository: repository.map(ToOwned::to_owned),
            namespace: namespace.map(ToOwned::to_owned),
        }
    }

    fn sign_cosign_payload(
        sha256: &str,
        docker_reference: &str,
        issuer: Option<&str>,
        repository: Option<&str>,
        namespace: Option<&str>,
    ) -> ArtifactSignature {
        let signing_key = SigningKey::from_bytes(&[8u8; 32]);
        let payload = serde_json::json!({
            "critical": {
                "identity": {
                    "docker-reference": docker_reference,
                },
                "image": {
                    "docker-manifest-digest": format!("sha256:{sha256}"),
                },
                "type": "cosign container image signature",
            },
            "optional": {
                "issuer": issuer,
                "repository": repository,
                "namespace": namespace,
            }
        })
        .to_string();
        let signature = signing_key.sign(payload.as_bytes());
        ArtifactSignature {
            algorithm: "cosign-ed25519".to_string(),
            public_key: STANDARD.encode(signing_key.verifying_key().to_bytes()),
            signature: STANDARD.encode(signature.to_bytes()),
            payload: Some(payload),
            issuer: issuer.map(ToOwned::to_owned),
            identity: None,
            repository: repository.map(ToOwned::to_owned),
            namespace: namespace.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn test_verify_artifact_signature_accepts_valid_signed_claims() {
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let artifact = RemoteArtifactSource {
            url: "https://example.test/app.wasm".to_string(),
            reference: None,
            sha256: sha256.to_string(),
            credential_ref: None,
            signature: Some(sign_artifact_claims(
                sha256,
                Some("https://token.actions.githubusercontent.com"),
                Some("example-org/hello"),
                Some("production"),
            )),
        };
        let verification = verify_artifact_signature(
            &SignaturePolicy {
                require_signature: true,
                allowed_issuers: vec!["https://token.actions.githubusercontent.com".to_string()],
                allowed_identities: Vec::new(),
                allowed_repositories: vec!["example-org/hello".to_string()],
                allowed_namespaces: vec!["production".to_string()],
            },
            sha256,
            &artifact,
        )
        .expect("signature should verify");

        assert!(verification.verified);
        assert_eq!(verification.algorithm.as_deref(), Some("ed25519"));
    }

    #[test]
    fn test_verify_artifact_signature_rejects_disallowed_repository() {
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let artifact = RemoteArtifactSource {
            url: "https://example.test/app.wasm".to_string(),
            reference: None,
            sha256: sha256.to_string(),
            credential_ref: None,
            signature: Some(sign_artifact_claims(
                sha256,
                Some("https://issuer.example"),
                Some("evil/repo"),
                Some("production"),
            )),
        };
        let err = verify_artifact_signature(
            &SignaturePolicy {
                require_signature: true,
                allowed_issuers: vec!["https://issuer.example".to_string()],
                allowed_identities: Vec::new(),
                allowed_repositories: vec!["example-org/hello".to_string()],
                allowed_namespaces: vec!["production".to_string()],
            },
            sha256,
            &artifact,
        )
        .expect_err("repository policy should reject signature");

        assert!(err.to_string().contains("repository claim"));
    }

    #[test]
    fn test_verify_artifact_signature_accepts_cosign_payload_mode() {
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let artifact = RemoteArtifactSource {
            url: "https://example.test/app.wasm".to_string(),
            reference: None,
            sha256: sha256.to_string(),
            credential_ref: None,
            signature: Some(sign_cosign_payload(
                sha256,
                "ghcr.io/example-org/hello:latest",
                Some("https://token.actions.githubusercontent.com"),
                Some("example-org/hello"),
                Some("production"),
            )),
        };

        let verification = verify_artifact_signature(
            &SignaturePolicy {
                require_signature: true,
                allowed_issuers: vec!["https://token.actions.githubusercontent.com".to_string()],
                allowed_identities: Vec::new(),
                allowed_repositories: vec!["example-org/hello".to_string()],
                allowed_namespaces: vec!["production".to_string()],
            },
            sha256,
            &artifact,
        )
        .expect("cosign signature should verify");

        assert!(verification.verified);
        assert_eq!(verification.algorithm.as_deref(), Some("cosign-ed25519"));
        assert_eq!(
            verification.repository.as_deref(),
            Some("example-org/hello")
        );
    }

    #[test]
    fn test_verify_artifact_signature_rejects_cosign_digest_mismatch() {
        let artifact = RemoteArtifactSource {
            url: "https://example.test/app.wasm".to_string(),
            reference: None,
            sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
            credential_ref: None,
            signature: Some(sign_cosign_payload(
                "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                "ghcr.io/example-org/hello:latest",
                Some("https://token.actions.githubusercontent.com"),
                Some("example-org/hello"),
                Some("production"),
            )),
        };

        let err = verify_artifact_signature(
            &SignaturePolicy {
                require_signature: true,
                allowed_issuers: vec!["https://token.actions.githubusercontent.com".to_string()],
                allowed_identities: Vec::new(),
                allowed_repositories: vec!["example-org/hello".to_string()],
                allowed_namespaces: vec!["production".to_string()],
            },
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            &artifact,
        )
        .expect_err("cosign digest mismatch should be rejected");

        assert!(err.to_string().contains("cosign payload digest mismatch"));
    }
}

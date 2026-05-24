# Secrets Crate

## Overview

The `secrets` crate provides encrypted secret storage for the Wasm Cloud Platform, implementing a two-tier encryption scheme (KEK → DEK → secret values) backed by a `redb` embedded database. It also offers X25519 ECDH key exchange combined with ChaCha20Poly1305 authenticated encryption for secure inter-node secret transfer during cluster bootstrap.

## Architecture

### Two-Tier Encryption Model

```
┌─────────────────────────────────────────────────┐
│                  Key Hierarchy                   │
├─────────────────────────────────────────────────┤
│                                                  │
│  KEK (Key Encryption Key)                        │
│  ├── Stored in redb (currently plaintext)        │
│  └── Encrypts DEKs                              │
│                                                  │
│  DEK (Data Encryption Key)                       │
│  ├── One per application                         │
│  ├── Encrypted by KEK in redb                    │
│  ├── Cached in memory (unbounded cache)          │
│  └── Encrypts secret values                      │
│                                                  │
│  Secret Values                                   │
│  ├── Encrypted by DEK using ChaCha20Poly1305     │
│  └── Stored as EncryptedBlob in redb             │
│                                                  │
└─────────────────────────────────────────────────┘
```

### Inter-Node Secret Transfer

During cluster bootstrap, secrets need to be securely transferred between nodes:

1. Each node generates an X25519 `BootstrapKeyPair`
2. Sender derives a shared secret via ECDH with recipient's public key
3. Secret is encrypted using ChaCha20Poly1305 with the shared secret
4. Recipient decrypts using their private key and sender's public key

> **Warning:** The current implementation uses the raw ECDH shared secret directly as an encryption key without a Key Derivation Function (KDF), which is not recommended.

### Storage Backend

Secrets are persisted in a `redb` embedded database with the following tables:
- KEK storage (plaintext)
- DEK storage (encrypted by KEK)
- Secret values (encrypted by DEK)

## Public API

### Core Trait

```rust
pub trait SecretProvider: Send + Sync {
    async fn get(&self, app_id: &str, key: &str) -> Result<String, PlatformError>;
    async fn set(&self, app_id: &str, key: &str, value: &str) -> Result<(), PlatformError>;
    async fn delete(&self, app_id: &str, key: &str) -> Result<(), PlatformError>;
}
```

### Key Types

| Type | Description |
|------|-------------|
| `SecretProvider` | Trait defining the secret storage interface |
| `LocalSecretProvider` | redb-backed implementation of `SecretProvider` |
| `BootstrapKeyPair` | X25519 key pair for inter-node secret transfer |
| `SymmetricKey` | Wrapper for symmetric encryption keys |
| `EncryptedBlob` | Container for encrypted data (ciphertext + nonce) |
| `AppSecretBundle` | Bundle containing app ID, encrypted DEK, and secrets |
| `encrypt_for_peer` | Function to encrypt secrets for a specific peer node |

### Example Usage

```rust
use secrets::{LocalSecretProvider, SecretProvider};

let provider = LocalSecretProvider::new(db_path)?;

// Store a secret
provider.set("my-app", "database-password", "s3cret!").await?;

// Retrieve a secret
let value = provider.get("my-app", "database-password").await?;

// Delete a secret
provider.delete("my-app", "database-password").await?;
```

## Known Issues & Improvements

### Critical

| Issue | Impact | Suggested Fix |
|-------|--------|---------------|
| TOCTOU race in `get_or_create_dek` | Concurrent DEK generation can cause data loss | Use atomic compare-and-swap or redb transaction isolation |
| Raw ECDH shared secret used as encryption key | Cryptographic weakness; violates NIST SP 800-56A | Apply HKDF or similar KDF to derive encryption key |
| KEK stored in plaintext in redb | Compromised database exposes all secrets | Encrypt KEK with a master key derived from environment/HSM |
| `encrypt_for_peer` and decrypt return empty vec on error | Silent data loss; impossible to distinguish success from failure | Return `Result` type with descriptive errors |

### High Priority

| Issue | Impact | Suggested Fix |
|-------|--------|---------------|
| `set()`/`delete()` lost-update race | Concurrent writes can overwrite each other | Implement optimistic concurrency control with version checks |
| `delete()` increments version on non-existent key | Incorrect versioning semantics | Check existence before deletion; return error or no-op |
| `get()` uses `AppNotFound` for missing secret key | Wrong error variant misleads callers | Add `SecretNotFound` variant to error enum |
| `SymmetricKey` is `Clone + ZeroizeOnDrop` | Key material proliferates via cloning; zeroize only destroys one copy | Remove `Clone` impl; use `Arc` if sharing is needed |
| `SymmetricKey::generate()` uses `thread_rng()` | Not cryptographically guaranteed on all platforms | Use `OsRng` for key generation |
| Unbounded DEK cache | Memory leak over time | Implement LRU cache with bounded size |
| Synchronous redb operations in async context | Blocks Tokio runtime | Use `tokio::task::spawn_blocking` for redb operations |

### Medium Priority

| Issue | Impact | Suggested Fix |
|-------|--------|---------------|
| `AppSecretBundle` derives `Debug` | Leaks encrypted DEK in logs | Implement manual `Debug` that redacts sensitive fields |
| `local_provider` bypasses `Store` methods | Inconsistent abstraction; harder to test | Route all operations through `Store` trait |
| No key rotation support | Compromised keys cannot be rotated | Implement KEK and DEK rotation APIs |
| No audit logging for secret access | No accountability for secret access | Add audit log entries for get/set/delete operations |
| No access control/authorization | Any code with provider reference can access all secrets | Implement RBAC or capability-based access control |
| `SecretProvider` only supports `String` values | Cannot store binary data (certificates, keys) | Change trait to support `Vec<u8>` or generic bytes |

### Low Priority

| Issue | Impact | Suggested Fix |
|-------|--------|---------------|
| Unused `hex` dependency | Unnecessary dependency bloat | Remove from `Cargo.toml` |
| Secret bundle encoding should remain versioned and documented | Persisted format changes need migration discipline | Keep `AppSecretBundle` versioned and migrate deliberately |

## Security Considerations

### Encryption

- **Algorithm:** ChaCha20Poly1305 provides authenticated encryption with associated data (AEAD), ensuring both confidentiality and integrity of secret values.
- **Key Derivation:** The current implementation lacks proper key derivation for ECDH shared secrets. A KDF such as HKDF-SHA256 should be applied to derive encryption keys from the raw shared secret.
- **Key Storage:** The KEK is stored in plaintext within the redb database. An attacker with database access can decrypt all secrets. Consider using a hardware security module (HSM) or deriving the KEK from a secure external source.

### Key Management

- **Key Proliferation:** `SymmetricKey` implements `Clone`, allowing key material to be copied freely in memory. Combined with `ZeroizeOnDrop`, only the last dropped copy is zeroized. Remove `Clone` to enforce single ownership of key material.
- **Random Number Generation:** `SymmetricKey::generate()` uses `thread_rng()` which may fall back to weak PRNGs on some platforms. Use `OsRng` for cryptographic key generation to ensure proper entropy.

### Concurrency

- **TOCTOU Vulnerability:** The `get_or_create_dek` function has a time-of-check-to-time-of-use race condition. Two concurrent calls can both observe "no DEK exists" and create separate DEKs, with one being overwritten. This can cause data loss if secrets encrypted with the lost DEK cannot be recovered.
- **Lost Updates:** The `set()` and `delete()` operations lack optimistic concurrency control, allowing concurrent modifications to overwrite each other silently.

### Operational Security

- **Audit Trail:** There is no audit logging for secret access operations. Without audit logs, it is impossible to detect unauthorized access or investigate security incidents.
- **Access Control:** The `SecretProvider` trait has no authorization mechanism. Any code holding a reference to the provider can access all secrets for all applications.
- **Key Rotation:** There is no mechanism to rotate compromised keys. Once a KEK or DEK is compromised, all secrets encrypted with it remain vulnerable.

### Data Integrity

- **Silent Failures:** The `encrypt_for_peer` and decrypt functions return empty vectors on error instead of `Result` types. This makes it impossible to distinguish between successful encryption of empty data and a cryptographic failure.
- **Error Information Leakage:** `AppSecretBundle` derives `Debug`, which may leak encrypted key material in log output.

# Secrets Crate

## Overview

The `secrets` crate provides encrypted secret storage for the Wasm Cloud Platform, implementing a two-tier encryption scheme (KEK → DEK → secret values) backed by a `redb` embedded database. Stored DEKs and values use AES-256-GCM. The crate also offers X25519 ECDH key exchange combined with ChaCha20Poly1305 authenticated encryption for inter-node secret transfer during cluster bootstrap.

## Architecture

### Two-Tier Encryption Model

```
┌─────────────────────────────────────────────────┐
│                  Key Hierarchy                   │
├─────────────────────────────────────────────────┤
│                                                  │
│  KEK (Key Encryption Key)                        │
│  ├── Supplied to LocalSecretProvider             │
│  └── Encrypts DEKs with AES-256-GCM             │
│                                                  │
│  DEK (Data Encryption Key)                       │
│  ├── One per application                         │
│  ├── Encrypted by KEK in redb                    │
│  ├── Cached in memory (unbounded cache)          │
│  └── Encrypts values with AES-256-GCM            │
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
- One CBOR bundle per application containing its KEK-encrypted DEK, encrypted values, version, and update timestamp

The KEK is supplied by the caller. Production nodes obtain it through the configured key-root mechanism; this crate does not create a plaintext KEK table.

## Public API

### Core Trait

```rust
pub trait SecretProvider: Send + Sync {
    async fn get(&self, app_id: &AppId, key: &str) -> Result<String, PlatformError>;
    async fn set(&self, app_id: &AppId, key: &str, value: &str) -> Result<(), PlatformError>;
    async fn delete(&self, app_id: &AppId, key: &str) -> Result<(), PlatformError>;
    async fn list_keys(&self, app_id: &AppId) -> Result<Vec<String>, PlatformError>;
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
use common::types::AppId;
use secrets::{crypto::SymmetricKey, LocalSecretProvider, SecretProvider};
use storage::Store;

let store = Store::open(db_path)?;
let provider = LocalSecretProvider::new(store, SymmetricKey::from_bytes(kek_bytes));
let app_id = AppId::new("my-app", "v1");

// Store a secret
provider.set(&app_id, "database-password", "s3cret!").await?;

// Retrieve a secret
let value = provider.get(&app_id, "database-password").await?;

// Delete a secret
provider.delete(&app_id, "database-password").await?;
```

## Known Issues & Improvements

### Critical

| Issue | Impact | Suggested Fix |
|-------|--------|---------------|
| Raw ECDH shared secret used as encryption key | Cryptographic weakness; violates NIST SP 800-56A | Apply HKDF or similar KDF to derive encryption key |
| Separate providers can race while creating an app bundle | The in-memory double-check lock coordinates clones of one provider, but independent providers sharing a store do not share that lock | Create the initial bundle with a transactional insert-if-absent operation |

### High Priority

| Issue | Impact | Suggested Fix |
|-------|--------|---------------|
| `set()`/`delete()` lost-update race | Concurrent writes can overwrite each other | Implement optimistic concurrency control with version checks |
| Version checking and saving use separate transactions | Two independent providers can both pass the version check before either commits, so cross-provider lost updates remain possible | Perform compare and replacement in one redb write transaction |
| Unbounded DEK cache | Memory leak over time | Implement LRU cache with bounded size |
| Synchronous redb operations in async context | Blocks Tokio runtime | Use `tokio::task::spawn_blocking` for redb operations |

### Medium Priority

| Issue | Impact | Suggested Fix |
|-------|--------|---------------|
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

- **Algorithms:** Stored DEKs and values use AES-256-GCM. Bootstrap peer envelopes use X25519 plus ChaCha20Poly1305. Both provide authenticated encryption for their ciphertexts.
- **Key Derivation:** The current implementation lacks proper key derivation for ECDH shared secrets. A KDF such as HKDF-SHA256 should be applied to derive encryption keys from the raw shared secret.
- **Key Storage:** `LocalSecretProvider` receives the KEK from its caller and keeps it in zeroizing memory. Platform production admission requires the configured external key-root workflow; database bundles contain the encrypted DEK rather than the plaintext KEK.

### Key Management

- **Key ownership:** `SymmetricKey` does not implement `Clone`, `Debug`, or serialization and zeroizes its bytes on drop. Shared ownership uses `Arc<SymmetricKey>`.
- **Random number generation:** key and nonce generation use the `rand`/AEAD crates' cryptographic RNG interfaces.

### Concurrency

- **Provider scope:** clones of one provider share the DEK cache lock, but separate provider instances do not. Initial bundle creation and optimistic version checks are not atomic redb compare-and-set operations across providers.

### Operational Security

- **Audit Trail:** There is no audit logging for secret access operations. Without audit logs, it is impossible to detect unauthorized access or investigate security incidents.
- **Access Control:** The `SecretProvider` trait has no authorization mechanism. Any code holding a reference to the provider can access all secrets for all applications.
- **Key Rotation:** There is no mechanism to rotate compromised keys. Once a KEK or DEK is compromised, all secrets encrypted with it remain vulnerable.

### Data Integrity

- **Explicit failures:** local and bootstrap encryption/decryption return `Result`; malformed peer keys, short ciphertexts, authentication failures, and invalid UTF-8 are reported.
- **Debug redaction:** `AppSecretBundle` has a manual `Debug` implementation that reports byte/key counts instead of ciphertext contents.

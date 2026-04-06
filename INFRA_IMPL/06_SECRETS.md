# Step 06 — Encrypted Secrets Management

## Goal
Implement a two-layer secret system:

---

## Context & Rationale

### The Problem This Solves

Secrets (database passwords, API keys, signing tokens) must never appear in plaintext on
disk or in logs. But they must be available within milliseconds at spawn time — the cold
start target is < 10ms, so fetching a secret from a remote Vault over the network is
not an option on the critical path.

This step builds a local encrypted cache that makes secrets instantly available from `redb`,
while ensuring the data is meaningless if the disk is stolen.

### Why Not Just Use HashiCorp Vault?

Vault is an excellent secrets manager for long-running services. For this platform it has
two problems:

1. **Network latency on cold start**: A round-trip to Vault (even on a LAN) takes 2–10ms.
   Combined with other spawn steps, this would push cold start past the 10ms target.
2. **Operational dependency**: If Vault is unreachable (network partition, Vault restart),
   no new Wasm instances can be spawned. The platform becomes unavailable for new traffic.

The solution: **Vault as a source of truth + local redb as a cache**. Vault (or any remote
`SecretProvider`) is queried when a secret is first set or rotated. The value is encrypted
locally and cached in redb. Subsequent reads are instant, local, and network-independent.

### Why DEK/KEK Instead of a Single Master Key?

Encrypting every secret with the same master key has a critical failure mode: if the master
key is ever compromised, **all secrets for all apps are exposed simultaneously**.

The DEK/KEK hierarchy limits the blast radius:

```
Node Master Key (KEK) — never stored on disk as plaintext
        │
        ▼ (encrypts)
Per-App Data Encryption Key (DEK) — stored encrypted on disk
        │
        ▼ (encrypts)
Individual secret values — stored encrypted on disk
```

If App A's DEK is compromised (e.g. via a bug in the decryption code), only App A's secrets
are exposed. App B's DEK is independent and unaffected. The master KEK is loaded from
environment or TPM at startup and held only in process memory — it is never written to disk.

### Why AES-256-GCM (and not ChaCha20-Poly1305)?

Both are AEAD (Authenticated Encryption with Associated Data) ciphers and are equally secure.
The choice is pragmatic:

- AES-256-GCM has hardware acceleration on all modern x86 CPUs (via AES-NI instructions).
  On a server, it is 3–5x faster than ChaCha20-Poly1305.
- ChaCha20-Poly1305 is preferred on mobile/embedded where AES-NI is absent.

Since this platform targets server nodes with modern CPUs, AES-256-GCM is the right default.
The `SecretProvider` trait allows swapping the cipher without changing calling code.

### Why Zeroize on SymmetricKey?

Key material in memory is a target for memory-scanning attacks (core dumps, `/proc/self/mem`
reads by a compromised co-tenant). The `#[derive(Zeroize, ZeroizeOnDrop)]` attribute on
`SymmetricKey` ensures the 32 key bytes are overwritten with zeros when the struct is
dropped — before the allocator can reclaim the memory for another use.

### NATS-Driven Rotation Without Restart

When a secret is rotated (e.g. a database password is changed), the flow is:

```
Operator: wasm-ctl secrets set api-users DATABASE_URL postgres://new-creds
       │
       ▼
wasm-ctl publishes "secrets.update.api-users:v2" to NATS
       │
       ▼ (all nodes subscribed to "secrets.update.>")
Each node's secret rotation handler:
  1. Decrypts the new value with the cluster key
  2. Re-encrypts with the node's own DEK
  3. Writes to redb [secrets table]
       │
       ▼
Next instance spawn picks up the new value automatically
Running instances: unaffected (they read env vars at spawn time, not live)
```

The running instance sees the old password until it is gracefully replaced by the hot-swap
deploy protocol. This is intentional — you don't want a running request to suddenly get a
new database connection mid-transaction.

---
- **Local cache** in `redb` with AES-GCM-SIV encryption (fast, offline-capable)
- **Remote source** via a `SecretProvider` trait (Vault, env file, etc.)
- **NATS-driven rotation** so updated secrets are picked up within seconds
- **DEK/KEK hierarchy** so that each app's secrets are encrypted with a unique key,
  which is itself encrypted by the node's master key

---

## 1. Encryption Primitives

```rust
// crates/secrets/src/crypto.rs
use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use zeroize::{Zeroize, ZeroizeOnDrop};
use common::error::PlatformError;

/// 256-bit symmetric key (32 bytes).
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SymmetricKey([u8; 32]);

impl SymmetricKey {
    /// Generate a new random key.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        use rand::RngCore;
        rand::thread_rng().fill_bytes(&mut bytes);
        SymmetricKey(bytes)
    }

    /// Load from raw bytes (e.g. from env or TPM).
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        SymmetricKey(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Encrypted blob: nonce (12 bytes) || ciphertext.
pub struct EncryptedBlob(pub Vec<u8>);

/// Encrypt plaintext with AES-256-GCM-SIV.
/// Nonce is prepended to the ciphertext.
pub fn encrypt(key: &SymmetricKey, plaintext: &[u8]) -> Result<EncryptedBlob, PlatformError> {
    let cipher = Aes256Gcm::new_from_slice(key.as_bytes())
        .map_err(|e| PlatformError::Encryption(e.to_string()))?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher.encrypt(&nonce, plaintext)
        .map_err(|e| PlatformError::Encryption(e.to_string()))?;

    let mut blob = nonce.to_vec();
    blob.extend_from_slice(&ciphertext);
    Ok(EncryptedBlob(blob))
}

/// Decrypt an EncryptedBlob (nonce || ciphertext).
pub fn decrypt(key: &SymmetricKey, blob: &EncryptedBlob) -> Result<Vec<u8>, PlatformError> {
    if blob.0.len() < 12 {
        return Err(PlatformError::Encryption("blob too short".into()));
    }
    let (nonce_bytes, ciphertext) = blob.0.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);
    let cipher = Aes256Gcm::new_from_slice(key.as_bytes())
        .map_err(|e| PlatformError::Encryption(e.to_string()))?;
    cipher.decrypt(nonce, ciphertext)
        .map_err(|e| PlatformError::Encryption(e.to_string()))
}
```

---

## 2. Key Hierarchy (KEK → DEK)

```
Node Master Key (KEK)
    │   Stored in memory only; sourced from:
    │   - Manual injection at startup (dev)
    │   - TPM chip (production)
    │   - NATS secure channel (cluster key sync)
    │
    ▼
Per-App Data Encryption Key (DEK)
    │   Generated on first deploy.
    │   Encrypted with KEK and stored in redb.
    │
    ▼
Per-App Secrets (plaintext in memory only during spawn)
    │   Encrypted with the app's DEK and stored in redb.
    │
    ▼
Injected into WasiEnv as env vars (in-process memory only)
```

```rust
// crates/secrets/src/crypto.rs (continued)
use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// Per-app encrypted secret bundle stored in redb.
#[derive(Debug, Serialize, Deserialize)]
pub struct AppSecretBundle {
    pub app_id: String,
    /// The DEK encrypted with the node's KEK.
    pub encrypted_dek: Vec<u8>,
    /// Map of secret_key → value encrypted with the DEK.
    pub secrets: HashMap<String, Vec<u8>>,
    /// Monotonically increasing version for rotation tracking.
    pub version: u64,
    /// Unix timestamp of last update.
    pub updated_at: u64,
}
```

---

## 3. SecretProvider Trait

```rust
// crates/secrets/src/provider.rs
use common::{error::PlatformError, types::AppId};
use async_trait::async_trait;

/// Abstraction over different secret backends.
/// The Supervisor uses this to get secret values at spawn time.
#[async_trait]
pub trait SecretProvider: Send + Sync + 'static {
    /// Get the plaintext value of a secret for the given app.
    async fn get(&self, app_id: &AppId, key: &str) -> Result<String, PlatformError>;

    /// Set (or update) a secret.
    async fn set(&self, app_id: &AppId, key: &str, value: &str) -> Result<(), PlatformError>;

    /// Delete a secret.
    async fn delete(&self, app_id: &AppId, key: &str) -> Result<(), PlatformError>;

    /// List all secret keys for an app.
    async fn list_keys(&self, app_id: &AppId) -> Result<Vec<String>, PlatformError>;
}
```

---

## 4. Local redb-backed SecretProvider

The main provider for production use. Reads/writes the encrypted bundle in `redb`.

```rust
// crates/secrets/src/local_provider.rs
use crate::{
    crypto::{decrypt, encrypt, AppSecretBundle, EncryptedBlob, SymmetricKey},
    provider::SecretProvider,
};
use async_trait::async_trait;
use bincode;
use common::{error::PlatformError, types::AppId};
use storage::{tables::SECRETS, Store};
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct LocalSecretProvider {
    store: Store,
    kek: Arc<SymmetricKey>,
    /// In-memory cache of decrypted DEKs (keyed by app_id).
    dek_cache: Arc<RwLock<std::collections::HashMap<String, SymmetricKey>>>,
}

impl LocalSecretProvider {
    pub fn new(store: Store, kek: SymmetricKey) -> Self {
        LocalSecretProvider {
            store,
            kek: Arc::new(kek),
            dek_cache: Default::default(),
        }
    }

    /// Get or create the DEK for an app.
    async fn get_or_create_dek(&self, app_id: &AppId) -> Result<SymmetricKey, PlatformError> {
        // Check in-memory cache first
        {
            let cache = self.dek_cache.read().await;
            if cache.contains_key(&app_id.0) {
                let dek_bytes = cache[&app_id.0].as_bytes().clone();
                return Ok(SymmetricKey::from_bytes(*dek_bytes));
            }
        }

        // Load bundle from redb
        let bundle = self.load_bundle(app_id)?;
        let dek = match bundle {
            Some(b) => {
                // Decrypt the DEK using the KEK
                let decrypted = decrypt(&self.kek, &EncryptedBlob(b.encrypted_dek))?;
                let mut key_bytes = [0u8; 32];
                key_bytes.copy_from_slice(&decrypted[..32]);
                SymmetricKey::from_bytes(key_bytes)
            }
            None => {
                // First time: generate a new DEK, encrypt it with KEK, store bundle
                let new_dek = SymmetricKey::generate();
                let encrypted_dek = encrypt(&self.kek, new_dek.as_bytes())?;
                let bundle = AppSecretBundle {
                    app_id: app_id.0.clone(),
                    encrypted_dek: encrypted_dek.0,
                    secrets: Default::default(),
                    version: 1,
                    updated_at: now_secs(),
                };
                self.save_bundle(&bundle)?;
                new_dek
            }
        };

        // Cache the DEK
        let mut cache = self.dek_cache.write().await;
        cache.insert(app_id.0.clone(), SymmetricKey::from_bytes(*dek.as_bytes()));

        Ok(dek)
    }

    fn load_bundle(&self, app_id: &AppId) -> Result<Option<AppSecretBundle>, PlatformError> {
        let tx = self.store.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(SECRETS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        match table.get(app_id.0.as_str())
            .map_err(|e| PlatformError::Storage(e.to_string()))? {
            Some(v) => {
                let bundle: AppSecretBundle = bincode::decode_from_slice(
                    v.value(),
                    bincode::config::standard(),
                ).map_err(|e| PlatformError::Storage(e.to_string()))?.0;
                Ok(Some(bundle))
            }
            None => Ok(None),
        }
    }

    fn save_bundle(&self, bundle: &AppSecretBundle) -> Result<(), PlatformError> {
        let bytes = bincode::encode_to_vec(bundle, bincode::config::standard())
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let tx = self.store.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(SECRETS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.insert(bundle.app_id.as_str(), bytes.as_slice())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
    }
}

#[async_trait]
impl SecretProvider for LocalSecretProvider {
    async fn get(&self, app_id: &AppId, key: &str) -> Result<String, PlatformError> {
        let dek = self.get_or_create_dek(app_id).await?;
        let bundle = self.load_bundle(app_id)?
            .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;
        let encrypted_value = bundle.secrets.get(key)
            .ok_or_else(|| PlatformError::AppNotFound(format!("secret '{key}' not found")))?;
        let plaintext = decrypt(&dek, &EncryptedBlob(encrypted_value.clone()))?;
        String::from_utf8(plaintext).map_err(|e| PlatformError::Encryption(e.to_string()))
    }

    async fn set(&self, app_id: &AppId, key: &str, value: &str) -> Result<(), PlatformError> {
        let dek = self.get_or_create_dek(app_id).await?;
        let encrypted_value = encrypt(&dek, value.as_bytes())?;
        let mut bundle = self.load_bundle(app_id)?
            .unwrap_or_else(|| AppSecretBundle {
                app_id: app_id.0.clone(),
                encrypted_dek: vec![],
                secrets: Default::default(),
                version: 1,
                updated_at: now_secs(),
            });
        bundle.secrets.insert(key.to_string(), encrypted_value.0);
        bundle.version += 1;
        bundle.updated_at = now_secs();
        self.save_bundle(&bundle)
    }

    async fn delete(&self, app_id: &AppId, key: &str) -> Result<(), PlatformError> {
        let mut bundle = self.load_bundle(app_id)?
            .ok_or_else(|| PlatformError::AppNotFound(app_id.0.clone()))?;
        bundle.secrets.remove(key);
        bundle.version += 1;
        self.save_bundle(&bundle)
    }

    async fn list_keys(&self, app_id: &AppId) -> Result<Vec<String>, PlatformError> {
        Ok(self.load_bundle(app_id)?
            .map(|b| b.secrets.keys().cloned().collect())
            .unwrap_or_default())
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
```

---

## 5. Node Master Key Bootstrap

How the KEK is loaded at node startup:

```rust
// crates/node/src/main.rs (key bootstrap)
use secrets::crypto::SymmetricKey;

fn load_master_key(config: &NodeConfig) -> SymmetricKey {
    match config.key_source.as_str() {
        "env" => {
            // Dev mode: read from environment variable
            let hex = std::env::var("NODE_MASTER_KEY")
                .expect("NODE_MASTER_KEY must be set in env key mode");
            let bytes = hex::decode(hex).expect("invalid hex key");
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes[..32]);
            SymmetricKey::from_bytes(arr)
        }
        "file" => {
            // Read from a key file with restricted permissions (chmod 600)
            let path = config.key_file.as_deref().expect("key_file required");
            let content = std::fs::read(path).expect("cannot read key file");
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&content[..32]);
            SymmetricKey::from_bytes(arr)
        }
        "generate" => {
            // First-run: generate and persist the key to disk
            let key = SymmetricKey::generate();
            let path = config.key_file.as_deref().unwrap_or("/etc/wasm-node/master.key");
            std::fs::write(path, key.as_bytes()).expect("cannot write key file");
            tracing::warn!("Generated new master key and saved to {path}. Back it up!");
            key
        }
        _ => panic!("Unknown key source: {}", config.key_source),
    }
}
```

---

## 6. Secret Rotation via NATS

When a secret is updated in the external Secret Manager, NATS broadcasts the invalidation.

```rust
// crates/messaging/src/handlers.rs (secret rotation handler)
use async_nats::Client;
use secrets::LocalSecretProvider;
use common::types::AppId;

/// Subject pattern: "secrets.update.<app_id>"
pub async fn handle_secret_rotation(
    nats: &Client,
    secret_provider: &LocalSecretProvider,
) {
    let mut sub = nats.subscribe("secrets.update.>").await.unwrap();
    while let Some(msg) = sub.next().await {
        // Subject: secrets.update.api-users:v2
        let app_id_str = msg.subject.strip_prefix("secrets.update.").unwrap_or("");
        let app_id = AppId(app_id_str.to_string());

        // The NATS message body contains JSON: { "key": "DATABASE_URL", "value": "postgres://..." }
        if let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
            let key = payload["key"].as_str().unwrap_or("");
            let value = payload["value"].as_str().unwrap_or("");
            if !key.is_empty() {
                match secret_provider.set(&app_id, key, value).await {
                    Ok(_) => tracing::info!(app = app_id_str, key, "secret rotated"),
                    Err(e) => tracing::error!(app = app_id_str, key, error = %e, "rotation failed"),
                }
            }
        }

        // Acknowledge (if using JetStream)
        msg.ack().await.ok();
    }
}
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### Crypto Primitives
- [ ] `SymmetricKey::generate()` produces 32 random bytes, different on each call
- [ ] `encrypt()` returns a blob of at least 13 bytes (12-byte nonce + ciphertext)
- [ ] `decrypt(key, encrypt(key, plaintext))` returns the original plaintext exactly
- [ ] `decrypt()` with the wrong key returns `Err` — never garbage data
- [ ] `SymmetricKey` is zeroed from memory on drop (verified via `zeroize` derive)
- [ ] `SymmetricKey` does NOT implement `Serialize` or `Debug` (prevents accidental logging)

### DEK / KEK Hierarchy
- [ ] A fresh app generates a new DEK on the first `set()` call
- [ ] The DEK is stored encrypted with the KEK in the `SECRETS` table — never in plaintext on disk
- [ ] `get()` for a known secret returns the exact plaintext that was `set()`
- [ ] `get()` for an unknown key returns `Err` — not empty string
- [ ] `delete()` removes a key; subsequent `get()` returns `Err`
- [ ] `list_keys()` returns key names only — never values

### Isolation
- [ ] App A and App B with the same key name (`DATABASE_URL`) hold independent encrypted values
- [ ] App A cannot call `get()` to retrieve App B's secrets even sharing the same `Store`

### NATS Rotation
- [ ] Publishing `secrets.update.<app_id>` causes the local provider to update redb
- [ ] The next instance spawned after rotation receives the new secret value
- [ ] Already-running instances are not disrupted during rotation

### Master Key Bootstrap
- [ ] `--key-source env` reads `NODE_MASTER_KEY` and fails fast with a clear message if missing
- [ ] `--key-source file` reads exactly 32 bytes; fails if file is shorter
- [ ] `--key-source generate` writes a new key file and logs a visible warning to back it up

### Tests
- [ ] Encrypt → decrypt roundtrip for 10 different plaintexts all pass
- [ ] Wrong-key decryption returns an error (not a panic)
- [ ] `LocalSecretProvider` set → get → delete → get returns error end-to-end test passes

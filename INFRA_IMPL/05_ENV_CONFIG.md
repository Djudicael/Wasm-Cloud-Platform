# Step 05 — Environment Variables & App Configuration

## Goal
Implement the full configuration pipeline: storing per-app config in `redb`, resolving env vars
at spawn time (merging static config with live secrets), and injecting them into the WasiEnv.

---

## Context & Rationale

### The Problem This Solves

A Wasm app is a self-contained binary. It cannot read a config file from the host filesystem
(WASI sandboxes the filesystem). It cannot call a metadata service like EC2's IMDSv2. The
only mechanism WASI provides for injecting runtime configuration is **environment variables**.

This step defines how configuration travels from the operator's CLI command all the way into
the running Wasm process, so that `std::env::var("DATABASE_URL")` works exactly as it would
on a native Linux machine — with no changes to the app's code.

### Why Separate Static Config and Secrets?

`AppConfig.env_vars` holds non-sensitive values (`LOG_LEVEL=debug`, `APP_VERSION=v2`).
`AppConfig.secret_keys` holds a **list of key names** — not values — like `["DATABASE_URL"]`.

The actual secret values are stored in an encrypted bundle in a separate `[secrets]` redb
table (step 06). At spawn time, `EnvResolver` reads the encrypted bundle, decrypts the
requested keys, and merges them with the static vars.

**Why this separation?**

1. **Auditability**: An operator can view `AppConfig` to see which secrets an app uses,
   without ever seeing the secret values themselves
2. **Rotation without redeploy**: When `DATABASE_URL` is rotated, only the secrets table
   changes. The `AppConfig` is untouched. The next instance spawn picks up the new value.
3. **Logging safety**: The `AppConfig` can be logged (for debugging) without risk of leaking
   credentials. The secret values never appear in configs.

### Priority Resolution (Why Secrets Override Static Vars)

The `EnvResolver::resolve()` merges in three layers, lowest priority first:

```
Layer 1: Static env_vars (from AppConfig)
         └── e.g. DATABASE_URL=postgres://localhost/dev

Layer 2: Resolved secrets (from [secrets] table)
         └── e.g. DATABASE_URL=postgres://prod-db/myapp  ← OVERRIDES layer 1

Layer 3: Platform-injected vars (always present, highest priority)
         └── PORT, HOST_PORT, APP_ID, INSTANCE_ID, NODE_ID
```

This ordering means a development default (`DATABASE_URL=localhost`) in the static config
is automatically overridden by the production secret in the secrets table, without any
code change.

### Zero-Downtime Config Updates

When a config changes, existing instances continue running with the old config. Only newly
spawned instances pick up the new values. This is the correct behavior because:

1. Changing config mid-request could cause inconsistent behavior (e.g., one request uses
   the old DB URL, the next uses the new one — pointing to different schemas)
2. Forcing a restart on config change would drop in-flight requests

The deploy protocol (step 10) handles the ordered drain: old instances finish their work,
new instances start with the new config. The transition is seamless.

### The PORT vs HOST_PORT Distinction

Two port values are injected:
- `PORT` = the port the Wasm app binds inside its virtual WASI environment (e.g. `8080`)
- `HOST_PORT` = the actual OS port the Supervisor allocated (e.g. `10347`)

The app code uses `PORT`. The Supervisor tracks `HOST_PORT` for registering in the
upstream table. This indirection lets the app developer write `bind(":8080")` in their
code regardless of what actual port gets allocated on the host.

---
The Wasm app sees a clean, standard `std::env::var()` interface — it has no idea where the
values came from.

---

## 1. Config Lifecycle

```
DEPLOY command (CLI / NATS)
        │
        ▼
  Supervisor receives AppConfig
        │
        ├──► save_config() → redb [configs table]
        │
        ▼
  On spawn request:
        │
        ├──► load_config() from redb
        ├──► load_secrets() from redb (encrypted, see step 06)
        ├──► merge_env_vars() → final Vec<(String, String)>
        │
        ▼
  WasiEnv::builder()
    .env("DATABASE_URL", "...")
    .env("PORT", "9347")
    .env("APP_VERSION", "v2")
    ...
        │
        ▼
  Wasm Module runs — reads env via std::env::var()
```

---

## 2. AppConfig Structure (Full Definition)

```rust
// crates/common/src/types.rs (extended)
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    /// Unique app identifier: "<name>:<version>"
    pub id: AppId,

    /// Maximum Fuel units per execution.
    pub fuel_quota: FuelQuota,

    /// Maximum linear memory pages (1 page = 64 KiB).
    pub memory_limit: MemoryPages,

    /// Maximum concurrent instances for this app on this node.
    pub max_instances: u32,

    /// Idle timeout: kill instance if no requests for this many seconds.
    pub idle_timeout_secs: u64,

    /// Port the Wasm app binds internally (usually 8080).
    pub wasm_bind_port: u16,

    /// Static environment variables (non-secret).
    /// Secrets are stored separately in the [secrets] table.
    pub env_vars: HashMap<String, String>,

    /// List of secret keys to inject (resolved from the secrets table).
    /// e.g. ["DATABASE_URL", "STRIPE_KEY"]
    pub secret_keys: Vec<String>,
}

impl AppConfig {
    /// Default safe config for a new app.
    pub fn default_for(app_id: AppId) -> Self {
        AppConfig {
            id: app_id,
            fuel_quota: FuelQuota(500_000_000),   // ~500ms of compute
            memory_limit: MemoryPages(2048),       // 128 MB
            max_instances: 10,
            idle_timeout_secs: 300,
            wasm_bind_port: 8080,
            env_vars: HashMap::new(),
            secret_keys: Vec::new(),
        }
    }
}
```

---

## 3. Config Operations in Storage

```rust
// crates/storage/src/config.rs
use crate::{Store, tables::CONFIGS};
use common::{error::PlatformError, types::{AppConfig, AppId}};

impl Store {
    /// Persist an AppConfig. Overwrites if it already exists (upsert semantics).
    pub fn save_config(&self, config: &AppConfig) -> Result<(), PlatformError> {
        let json = serde_json::to_string(config)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let tx = self.db.begin_write()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        {
            let mut table = tx.open_table(CONFIGS)
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
            table.insert(config.id.0.as_str(), json.as_str())
                .map_err(|e| PlatformError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| PlatformError::Storage(e.to_string()))
    }

    pub fn load_config(&self, id: &AppId) -> Result<Option<AppConfig>, PlatformError> {
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(CONFIGS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        match table.get(id.0.as_str())
            .map_err(|e| PlatformError::Storage(e.to_string()))? {
            Some(v) => Ok(Some(serde_json::from_str(v.value())
                .map_err(|e| PlatformError::Storage(e.to_string()))?)),
            None => Ok(None),
        }
    }

    /// Update a single env var without touching the rest of the config.
    /// Safe for live config updates without restart.
    pub fn set_env_var(
        &self,
        id: &AppId,
        key: &str,
        value: &str,
    ) -> Result<(), PlatformError> {
        let mut config = self.load_config(id)?
            .ok_or_else(|| PlatformError::AppNotFound(id.0.clone()))?;
        config.env_vars.insert(key.to_string(), value.to_string());
        self.save_config(&config)
    }

    /// List all configured apps.
    pub fn list_apps(&self) -> Result<Vec<AppId>, PlatformError> {
        let tx = self.db.begin_read()
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let table = tx.open_table(CONFIGS)
            .map_err(|e| PlatformError::Storage(e.to_string()))?;
        let mut ids = Vec::new();
        for entry in table.iter()
            .map_err(|e| PlatformError::Storage(e.to_string()))? {
            let (k, _) = entry.map_err(|e| PlatformError::Storage(e.to_string()))?;
            ids.push(AppId(k.value().to_string()));
        }
        Ok(ids)
    }
}
```

---

## 4. Env Var Resolver

Merges static config env vars with decrypted secrets into the final list passed to WasiEnv.

```rust
// crates/supervisor/src/env_resolver.rs
use common::{error::PlatformError, types::AppConfig};
use storage::Store;
use secrets::SecretProvider;

pub struct EnvResolver<S: SecretProvider> {
    store: Store,
    secret_provider: S,
}

impl<S: SecretProvider> EnvResolver<S> {
    pub fn new(store: Store, secret_provider: S) -> Self {
        EnvResolver { store, secret_provider }
    }

    /// Resolve the full environment for an app at spawn time.
    /// Returns a flat Vec<(key, value)> ready for WasiEnv injection.
    pub async fn resolve(
        &self,
        config: &AppConfig,
        host_port: u16,
    ) -> Result<Vec<(String, String)>, PlatformError> {
        let mut env: Vec<(String, String)> = Vec::new();

        // 1. Static vars from config (lowest priority)
        for (k, v) in &config.env_vars {
            env.push((k.clone(), v.clone()));
        }

        // 2. Resolved secrets (override static vars if key collides)
        for secret_key in &config.secret_keys {
            match self.secret_provider.get(&config.id, secret_key).await {
                Ok(value) => {
                    // Remove any duplicate from static vars
                    env.retain(|(k, _)| k != secret_key);
                    env.push((secret_key.clone(), value));
                }
                Err(e) => {
                    tracing::warn!(
                        app = %config.id.0,
                        key = secret_key,
                        error = %e,
                        "secret not found, skipping"
                    );
                }
            }
        }

        // 3. Platform-injected vars (highest priority, always override)
        env.push(("PORT".to_string(), config.wasm_bind_port.to_string()));
        env.push(("HOST_PORT".to_string(), host_port.to_string()));
        env.push(("APP_ID".to_string(), config.id.0.clone()));

        Ok(env)
    }
}
```

---

## 5. Config Update (Zero-Downtime)

When a config changes (e.g. a new env var is set), the Supervisor applies it on the next
instance spawn. Existing instances are not forcefully restarted.

```rust
// crates/supervisor/src/instance_manager.rs (config update handler)
use common::types::{AppId, AppConfig};
use storage::Store;

pub async fn handle_config_update(
    store: &Store,
    app_id: &AppId,
    new_config: AppConfig,
) -> Result<(), common::error::PlatformError> {
    // 1. Persist the new config
    store.save_config(&new_config)?;
    tracing::info!(app = %app_id.0, "config updated, effective on next instance spawn");

    // 2. Mark existing instances for graceful drain:
    //    - They finish their current requests.
    //    - New requests go to newly spawned instances that pick up the new config.
    //    (See step 10: Deployment Protocol for the full drain logic)

    Ok(())
}
```

---

## 6. App-Side Usage (Axum / Wasm)

The Wasm app reads config exactly as it would on a native Linux machine.
**No platform SDK required in the app code.**

```rust
// apps/hello-axum/src/main.rs
use axum::{routing::get, Router};
use std::env;

#[tokio::main]
async fn main() {
    // Reads the PORT env var injected by the Supervisor
    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .expect("PORT must be a number");

    // Database URL injected as a secret
    let db_url = env::var("DATABASE_URL")
        .expect("DATABASE_URL is required");

    let app = Router::new()
        .route("/", get(|| async { "Hello from Wasm!" }))
        .route("/health", get(|| async { "OK" }));

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("failed to bind");

    axum::serve(listener, app).await.unwrap();
}
```

---

## 7. Reserved Environment Variables

These are always set by the Supervisor. The app config must not override them.

| Variable      | Set by     | Value                                                |
| ------------- | ---------- | ---------------------------------------------------- |
| `PORT`        | Supervisor | The `wasm_bind_port` from AppConfig (e.g. `8080`)    |
| `HOST_PORT`   | Supervisor | The actual host-side port allocated by PortAllocator |
| `APP_ID`      | Supervisor | The full app ID string (e.g. `api-users:v2`)         |
| `INSTANCE_ID` | Supervisor | UUID of this specific instance                       |
| `NODE_ID`     | Supervisor | Unique ID of this node in the cluster                |

---

## 8. Deploy-Time Config Validation

### The Problem

`EnvResolver::resolve()` logs a warning when a secret key is missing but continues the
spawn. This is intentional during normal operation (a secret rotation might have a brief
window where the old key is deleted before the new one is set). However, at **deploy time**
— when an operator deploys a new app or a new version — every declared secret must already
exist. Otherwise the operator silently ships a broken deployment.

### Validation Rules

Config validation runs **once** when a `DEPLOY` command arrives, before the first instance
is ever spawned. It does not run on subsequent spawns (those use the "warn and skip"
behavior to tolerate transient secret rotation windows).

```rust
// crates/supervisor/src/config_validator.rs
use common::{error::PlatformError, types::AppConfig};
use secrets::SecretProvider;

#[derive(Debug)]
pub struct ConfigValidationError {
    pub missing_secrets: Vec<String>,
    pub reserved_conflicts: Vec<String>,
    pub invalid_fields: Vec<String>,
}

impl std::fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.missing_secrets.is_empty() {
            writeln!(f, "missing secrets: {}", self.missing_secrets.join(", "))?;
        }
        if !self.reserved_conflicts.is_empty() {
            writeln!(
                f,
                "env_vars conflict with reserved names: {}",
                self.reserved_conflicts.join(", ")
            )?;
        }
        if !self.invalid_fields.is_empty() {
            for msg in &self.invalid_fields {
                writeln!(f, "invalid config: {msg}")?;
            }
        }
        Ok(())
    }
}

/// Reserved env var names that the platform always injects.
/// Apps must not set these in env_vars or secret_keys.
const RESERVED_VARS: &[&str] = &["PORT", "HOST_PORT", "APP_ID", "INSTANCE_ID", "NODE_ID"];

/// Validate an AppConfig before accepting a deployment.
/// Returns Ok(()) if the config is valid; Err with a detailed breakdown otherwise.
pub async fn validate_config<S: SecretProvider>(
    config: &AppConfig,
    secret_provider: &S,
) -> Result<(), PlatformError> {
    let mut error = ConfigValidationError {
        missing_secrets: Vec::new(),
        reserved_conflicts: Vec::new(),
        invalid_fields: Vec::new(),
    };

    // 1. Check that all declared secret_keys actually exist in the secrets store.
    for key in &config.secret_keys {
        if secret_provider.get(&config.id, key).await.is_err() {
            error.missing_secrets.push(key.clone());
        }
    }

    // 2. Check that env_vars do not collide with reserved platform variables.
    for key in config.env_vars.keys() {
        if RESERVED_VARS.contains(&key.as_str()) {
            error.reserved_conflicts.push(key.clone());
        }
    }

    // Also check secret_keys for reserved name collisions.
    for key in &config.secret_keys {
        if RESERVED_VARS.contains(&key.as_str()) {
            error.reserved_conflicts.push(key.clone());
        }
    }

    // 3. Sanity-check numeric fields.
    if config.max_instances == 0 {
        error.invalid_fields.push("max_instances must be > 0".to_string());
    }
    if config.idle_timeout_secs == 0 {
        error.invalid_fields.push("idle_timeout_secs must be > 0".to_string());
    }
    if config.wasm_bind_port == 0 {
        error.invalid_fields.push("wasm_bind_port must be > 0".to_string());
    }
    if config.fuel_quota.0 == 0 {
        error.invalid_fields.push("fuel_quota must be > 0".to_string());
    }
    if config.memory_limit.0 == 0 {
        error.invalid_fields.push("memory_limit must be > 0".to_string());
    }

    // 4. Fail if any issues were found.
    let has_errors = !error.missing_secrets.is_empty()
        || !error.reserved_conflicts.is_empty()
        || !error.invalid_fields.is_empty();

    if has_errors {
        Err(PlatformError::ConfigValidation(error.to_string()))
    } else {
        Ok(())
    }
}
```

### Integration with the Deploy Path

The Supervisor calls `validate_config()` in the deploy handler **before** persisting the
config. A failed validation returns an error to the caller (CLI or NATS reply) with the
full list of issues.

```rust
// crates/supervisor/src/deploy.rs (updated excerpt)
use crate::config_validator::validate_config;

pub async fn handle_deploy(
    store: &Store,
    secret_provider: &impl SecretProvider,
    config: AppConfig,
    wasm_bytes: Vec<u8>,
) -> Result<(), PlatformError> {
    // Validate BEFORE persisting anything.
    validate_config(&config, secret_provider).await?;

    // Validation passed — proceed with the normal deploy flow.
    store.save_config(&config)?;
    store.save_artifact(&config.id, &wasm_bytes)?;
    // ... (rest of deploy logic from step 10)
    Ok(())
}
```

### CLI Error Output

When validation fails, the operator sees every issue at once, not one at a time:

```
$ wasm-ctl deploy myapp:v3 --wasm ./app.wasm --secret-keys DATABASE_URL,STRIPE_KEY
Error: deploy failed — config validation errors:
  missing secrets: STRIPE_KEY
  env_vars conflict with reserved names: PORT
  invalid config: max_instances must be > 0
```

---

## Completion Checklist

**This step is done when all boxes are checked.**

### AppConfig
- [ ] `AppConfig` includes all fields: `id`, `fuel_quota`, `memory_limit`, `max_instances`, `idle_timeout_secs`, `wasm_bind_port`, `env_vars`, `secret_keys`
- [ ] `AppConfig::default_for(app_id)` produces sensible defaults (500M fuel, 128MB RAM, 10 instances)
- [ ] `AppConfig` serializes and deserializes through JSON with no field loss

### Config Storage
- [ ] `save_config()` persists the config to the `CONFIGS` redb table
- [ ] `load_config()` returns `None` for an unknown app (not an error)
- [ ] `set_env_var()` updates a single key without overwriting other fields
- [ ] `list_apps()` returns all previously saved app IDs

### Env Resolver
- [ ] `EnvResolver::resolve()` merges static vars + resolved secrets into a flat `Vec<(String, String)>`
- [ ] Secrets override static vars when keys collide
- [ ] Platform vars (`PORT`, `APP_ID`, `INSTANCE_ID`, `NODE_ID`) are always present and always override app vars
- [ ] A missing secret key logs a warning but does not fail the spawn
- [ ] The resolved env vec contains no duplicate keys (last write wins)

### Live Config Update
- [ ] Calling `save_config()` with a modified config does not affect currently running instances
- [ ] The next instance spawned after a config update picks up the new values
- [ ] Config changes are visible to `load_config()` immediately after `save_config()` returns

### Config Validation
- [ ] `validate_config()` rejects a deploy when any declared `secret_keys` entry is missing from the secrets store
- [ ] `validate_config()` rejects env_vars or secret_keys that collide with reserved names (`PORT`, `HOST_PORT`, `APP_ID`, `INSTANCE_ID`, `NODE_ID`)
- [ ] `validate_config()` rejects zero-value numeric fields (`max_instances`, `idle_timeout_secs`, `wasm_bind_port`, `fuel_quota`, `memory_limit`)
- [ ] Validation errors list **all** issues at once rather than failing on the first
- [ ] Validation runs at deploy time only — subsequent spawns still use the "warn and skip" behavior for missing secrets
- [ ] A failed validation returns the full error to the CLI caller without persisting the config

### Tests
- [ ] A test verifies that `PORT` is always present in the resolved env even if not in `env_vars`
- [ ] A test verifies that a secret value overrides a same-named static env var
- [ ] A test verifies that `validate_config()` returns all issues in a single error (missing secret + reserved conflict + invalid field)
- [ ] A test verifies that a valid config passes validation and proceeds to persist

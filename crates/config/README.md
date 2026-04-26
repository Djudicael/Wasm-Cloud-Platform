# `wasm-cloud-config`

Configuration management for the Wasm Cloud Platform, providing both cold (startup) and hot (runtime) configuration loading and updates.

## Overview

This crate handles all configuration concerns for the platform:

- **Cold config loading** — Loads configuration at startup using a layered merge strategy: built-in defaults → TOML file → environment variables → CLI overrides.
- **Hot-reloadable configuration** — Supports runtime configuration updates via `HotConfigHandle`, allowing certain settings to change without restarting the node.
- **Environment variable integration** — Automatically reads and applies environment variable overrides.
- **CLI override support** — Command-line arguments take the highest precedence in the merge chain.

## Architecture

### Cold Config Loading Pipeline

```
Defaults (hardcoded)
    │
    ▼
TOML Config File
    │
    ▼
Environment Variables
    │
    ▼
CLI Overrides
    │
    ▼
Final Resolved Config
```

Each layer overwrites values from the previous one. Later layers always take precedence.

### Hot Config Architecture

```
┌─────────────────┐     ┌──────────────────┐
│  HotConfigUpdate │────▶│  HotConfigHandle  │
│  (caller side)   │     │  (RwLock guard)   │
└─────────────────┘     └────────┬─────────┘
                                 │
                                 ▼
                        ┌─────────────────┐
                        │    HotConfig     │
                        │  (active config) │
                        └─────────────────┘
```

Hot configuration allows runtime updates to a subset of settings. Updates are applied atomically through the handle and can be reset to cold defaults.

## Public API

### Key Types

| Type | Description |
|------|-------------|
| `CliOverrides` | Represents command-line override values for configuration fields |
| `load_config()` | Main entry point; loads and merges configuration from all sources |
| `HotConfig` | The hot-reloadable configuration structure |
| `HotConfigHandle` | Handle to read, update, or reset hot configuration at runtime |
| `HotConfigUpdate` | Represents a partial update to apply to hot configuration |

### Primary Functions

- **`load_config()`** — Synchronously loads configuration from defaults, TOML file, environment variables, and CLI overrides. Returns the fully resolved configuration.

### Hot Config Operations

- **`HotConfigHandle::read()`** — Acquires a read lock and returns a guard to the current hot config.
- **`HotConfigHandle::apply_update()`** — Applies a `HotConfigUpdate` to the current hot configuration.
- **`HotConfigHandle::reset()`** — Resets hot configuration back to the cold defaults.

## Known Issues & Improvements

### Critical

| Issue | Impact | Recommendation |
|-------|--------|----------------|
| `merge_config` overwrites non-Option fields with TOML defaults | Partial TOML files reset unset fields to defaults, silently discarding values from earlier layers | Change merge strategy to only overwrite fields explicitly present in TOML; use `Option` wrappers or serde defaults |
| Corrupted persisted hot config prevents node startup | No fallback mechanism; node cannot start if hot config file is malformed | Add fallback to cold defaults when hot config fails to load; log a warning |
| `HotConfigHandle::read()/apply_update()/reset()` can panic on poisoned lock | A panic in any thread holding the lock crashes all subsequent operations | Return `Result` types instead of unwrapping; handle poisoned locks gracefully |

### High

| Issue | Impact | Recommendation |
|-------|--------|----------------|
| Invalid env var values silently ignored | Misconfigured environment variables produce no warning; node runs with unexpected defaults | Log warnings when env var parsing fails; consider failing fast on invalid values |
| No validation of TLS cert/key file existence | Node may start and fail later when attempting TLS handshake | Validate file existence and readability during `load_config()` |
| `load_config` is synchronous (blocking file I/O) | Blocks the async runtime during configuration loading | Provide an async version or document that it should be called before runtime start |

### Medium

| Issue | Impact | Recommendation |
|-------|--------|----------------|
| `merge_hot_config` name is misleading | Function just returns an overlay; does not actually merge | Rename to `compute_hot_overlay` or similar to clarify behavior |
| `merge_config` has no compile-time guarantee all fields are covered | Adding new config fields may be silently missed in merge logic | Use a derive macro or exhaustive match pattern to ensure all fields are handled |
| Unused dependencies: `async-nats`, `metrics`; excessive `tokio` features | Increases compile time and binary size unnecessarily | Remove unused deps; trim tokio to only required features |

### Low

| Issue | Impact | Recommendation |
|-------|--------|----------------|
| Env var tests not thread-safe | Parallel test runs may interfere with each other's env var state | Use serial test execution or per-test isolation for env var tests |

## Security Considerations

1. **Environment variable exposure** — Environment variables may be visible in process listings. Avoid putting secrets directly in environment variables; prefer secret management integrations.

2. **TOML file permissions** — The configuration file may contain sensitive paths and settings. Ensure file permissions are restrictive (e.g., `0600` on Unix).

3. **Hot config persistence** — Hot configuration changes are persisted to disk. Ensure the persistence path has appropriate access controls to prevent unauthorized modification.

4. **No TLS validation at config time** — TLS certificate and key file paths are not validated for existence or readability during configuration loading. A misconfiguration may not surface until runtime, potentially leaving the node in an insecure state (falling back to non-TLS if not carefully handled).

5. **Lock poisoning** — The `RwLock` inside `HotConfigHandle` can become poisoned if a thread panics while holding the lock. Current code unwraps the lock, which will propagate the panic. This could be exploited for denial-of-service if an attacker can trigger a panic in code holding the lock.

# common

The shared foundation library for the Wasm Cloud Platform. This crate provides core types, configuration primitives, authentication, logging, policy enforcement, billing, health checking, and protocol versioning used across all other crates in the workspace.

## Overview

`common` serves as the single source of truth for cross-cutting concerns that every other crate depends on. It defines the shared vocabulary of types, error handling, and behavioral contracts that keep the platform consistent. No other crate in the workspace should redefine concepts that live here.

Key responsibilities:

- **Authentication & authorization** – token types, auth modes, and identity primitives
- **Billing** – billing record types and sequencing
- **Configuration** – shared config structs and defaults
- **Cryptography** – key management, hashing, and comparison utilities
- **Error handling** – the canonical `PlatformError` type
- **Garbage collection** – GC configuration and policy types
- **Health checking** – health status types and check interfaces
- **Logging** – structured logging setup and module-level filtering
- **Policy** – policy profiles and enforcement configuration
- **Protocol** – version negotiation and compatibility types
- **Types** – foundational domain types (`AppId`, `MemoryPages`, etc.)

## Architecture

```
common
├── auth        # Authentication modes, token types, identity
├── billing     # Billing record types and sequence tracking
├── config      # Shared configuration structs and defaults
├── crypto      # Cryptographic utilities, hashing, key comparison
├── error       # Canonical PlatformError and error conversion
├── gc          # Garbage collection configuration and policy
├── health      # Health status types and check interfaces
├── logging     # Structured logging initialization and filtering
├── policy      # Policy profiles and enforcement configuration
├── protocol    # Protocol version negotiation and compatibility
└── types       # Core domain types (AppId, MemoryPages, etc.)
```

All modules are designed to be consumed independently; there are no circular dependencies within the crate.

## Public API

### Key Types

| Type | Module | Description |
|------|--------|-------------|
| `AppId` | `types` | Validated application identifier |
| `MemoryPages` | `types` | Wasm memory page count with conversion helpers |
| `TokenType` | `auth` | Discriminant for token kinds (Read, Write, Admin) |
| `AuthMode` | `auth` | Authentication enforcement mode (Disabled, Legacy, Required) |
| `PolicyProfile` | `policy` | Predefined policy templates (StaticSite, FullApp, etc.) |
| `GcConfig` | `gc` | Garbage collection configuration (code-side) |
| `GcSection` | `gc` | Garbage collection TOML section (config-side) |
| `HealthStatus` | `health` | Node health state discriminant |
| `PlatformError` | `error` | Canonical error type for the platform |
| `ProtocolVersion` | `protocol` | Supported protocol version descriptor |
| `BillingRecord` | `billing` | Single billing event record |
| `ExtendedLimits` | `config` | Extended resource limits for privileged apps |

### Key Functions

| Function | Module | Description |
|----------|--------|-------------|
| `set_module_level()` | `logging` | Override the log level for a specific module |
| `init_logging()` | `logging` | Initialize the global logger with configured filters |
| `resolve()` | `policy` | Resolve a `PolicyProfile` into a concrete `PolicyConfig` |
| `check_memory()` | `health` | Check available system memory |

## Known Issues & Improvements

### Logging

- **`set_module_level()` replaces entire filter instead of adding module override.** Calling this function overwrites the global filter rather than inserting a per-module directive, making it impossible to selectively override one module's level while preserving others.

### Policy

- **`PolicyProfile::StaticSite` produces config that `resolve()` rejects.** The profile sets `max_outbound_connections=0`, which `resolve()` considers invalid, causing a runtime error for anyone using this profile.

### Authentication

- **`TokenType::ReadToken` used as placeholder on failed auth.** When authentication fails, the code falls back to `TokenType::ReadToken` instead of returning an error or a dedicated "unauthenticated" variant, which can grant unintended read access.
- **Auth disabled by default; legacy mode disables TLS.** The default `AuthMode::Disabled` means nodes start without authentication, and the legacy compatibility mode also turns off TLS, leaving the node completely unprotected.

### Garbage Collection

- **Inconsistent defaults between `GcConfig` and `GcSection`.** The code-side `GcConfig` and the config-file-side `GcSection` define different default values for the same fields, causing surprising behavior when only one side is explicitly set.

### Types

- **`AppId::new()` panics on invalid input instead of returning `Result`.** Any invalid string passed to `AppId::new()` causes a panic rather than a recoverable error, making it unsafe to use with user-supplied data.
- **`AppId(pub String)` inner field is public, bypassing validation.** The inner `String` of `AppId` is declared `pub`, allowing anyone to construct an `AppId` with arbitrary content and completely bypass the validation that `AppId::new()` performs.
- **`MemoryPages::to_bytes()` can overflow on 32-bit.** The conversion from page count to byte count can overflow on 32-bit targets for large page counts, producing incorrect results silently.
- **`ExtendedLimits` doesn't derive `Serialize`/`Deserialize`.** This type cannot be serialized or deserialized, preventing it from being used in persisted configurations or API responses.

### Cryptography

- **Constant-time comparison in `crypto.rs` is not truly constant-time for different lengths.** The comparison function short-circuits or behaves differently when the two inputs have different lengths, leaking length information through timing side-channels.

### Configuration

- **Default storage path uses `/tmp` (lost on reboot).** The default path for persistent storage is in `/tmp`, which is cleared on reboot, causing all persisted data to be lost.
- **No `validate()` method on `NodeConfig`.** There is no way to validate a `NodeConfig` after construction, so invalid configurations can propagate through the system until they cause runtime failures.

### Health

- **`check_memory()` does blocking file I/O.** The memory check reads from `/proc/meminfo` (or equivalent) synchronously, which can block an async runtime if called from an async context.

### Billing & Auditing

- **Audit logger drops records under backpressure.** When the audit logger cannot keep up with the incoming rate of records, it silently drops them instead of applying backpressure or persisting them for later delivery.

## Security Considerations

1. **Authentication is disabled by default.** Deployments must explicitly enable authentication. The legacy mode that disables TLS alongside auth is especially dangerous on any network-exposed node.

2. **`AppId` validation can be bypassed** via the public inner field. Any code with access to the type can construct an invalid `AppId`, potentially bypassing authorization checks that rely on well-formed identifiers.

3. **Timing side-channel in crypto comparison.** The non-constant-time comparison for different-length inputs can leak information about secret lengths. Use a constant-time comparison library (e.g., `subtle`) for all security-sensitive comparisons.

4. **`TokenType::ReadToken` fallback on auth failure** may grant read access to unauthenticated callers. Authentication failures should result in a denial, not a downgrade to read access.

5. **Default `/tmp` storage** means secrets, KEKs, and other sensitive data may be stored in a world-readable temporary directory that is cleared on reboot, leading to both data loss and potential exposure.

6. **`AppId::new()` panics** on invalid input, which can be triggered by untrusted data and used as a denial-of-service vector.

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
| `AuthConfig` / `Permission` | `auth` | Admin bearer-token policy, trusted proxies, and read/write authorization result |
| `PolicyProfile` | `policy` | Predefined policy templates such as HttpApi, BackgroundWorker, StaticSite, and DatabaseProxy |
| `GcConfig` | `gc` | Garbage collection configuration (code-side) |
| `GcSection` | `config` | Garbage collection TOML section (config-side) |
| `HealthStatus` | `health` | Node health state discriminant |
| `PlatformError` | `error` | Canonical error type for the platform |
| `ProtocolVersion` | `protocol` | Supported protocol version descriptor |
| `BillingRecord` | `billing` | Single billing event record |
| `ExtendedLimits` | `types` | Runtime resource limits for an application |

### Key Functions

| Function | Module | Description |
|----------|--------|-------------|
| `LogReloadHandle::set_module_level()` | `logging` | Add or replace a module-level logging directive while retaining the current filter |
| `init_logging()` | `logging` | Initialize the global logger with configured filters |
| `PolicyConfig::resolve()` | `policy` | Validate operator policy overrides and resolve an instance policy |
| `check_memory()` | `health` | Check available system memory |

## Known Issues & Improvements

### Authentication

- **The reusable `AuthConfig` defaults to disabled for local compatibility.** Production node admission rejects that configuration and requires authentication, TLS, and strong fixed-size bearer tokens. Standalone consumers of this crate must enforce an equivalent policy.

### Types

- **`AppId::new()` panics on invalid input instead of returning `Result`.** Any invalid string passed to `AppId::new()` causes a panic rather than a recoverable error, making it unsafe to use with user-supplied data.
- **`AppId(pub String)` inner field is public, bypassing validation.** The inner `String` of `AppId` is declared `pub`, allowing anyone to construct an `AppId` with arbitrary content and completely bypass the validation that `AppId::new()` performs.

### Configuration

- **The local-development storage default uses `/tmp`.** Production admission requires an explicit non-temporary storage path; standalone consumers must make the same distinction.
- **No `validate()` method on `NodeConfig`.** There is no way to validate a `NodeConfig` after construction, so invalid configurations can propagate through the system until they cause runtime failures.

### Health

- **`check_memory()` does blocking file I/O.** The memory check reads from `/proc/meminfo` (or equivalent) synchronously, which can block an async runtime if called from an async context.

### Billing & Auditing

- **Audit logger drops records under backpressure.** The bounded channel increments a dropped-record counter and emits periodic warnings, but it does not apply backpressure or persist records for later delivery.

## Security Considerations

1. **Authentication is disabled by default in the reusable config.** Production node admission rejects this setting; other binaries embedding `common` must enforce authentication and TLS before exposing an admin endpoint.

2. **`AppId` validation can be bypassed** via the public inner field. Any code with access to the type can construct an invalid `AppId`, potentially bypassing authorization checks that rely on well-formed identifiers.

3. **The `/tmp` storage default is for local development.** Production admission requires an explicit durable path, and operators must set restrictive ownership and permissions.

4. **`AppId::new()` panics** on invalid input. Use `AppId::new_validate()` for untrusted strings so invalid identifiers produce a recoverable error.

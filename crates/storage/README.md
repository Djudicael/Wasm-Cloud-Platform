# Storage Crate

## Overview

The `storage` crate provides a persistent key-value store layer built on [redb](https://github.com/cberner/redb), a simple, lightweight, ACID-compliant embedded database. It serves as the central data persistence layer for the Wasm Cloud Platform, managing compiled Wasm artifacts, raw binaries, application configurations, encrypted secrets, key encryption keys (KEK), telemetry metrics, routes, billing records, gateway configurations, and API keys.

Key responsibilities include:

- **Artifact Storage**: Persisting compiled Wasm modules and raw binaries with integrity verification
- **Configuration Management**: Storing and retrieving application and gateway configurations
- **Secrets Management**: Storing encrypted secrets and the key encryption key
- **Billing & Metrics**: Recording billing events and telemetry data
- **Schema Migration**: Handling database schema evolution across versions
- **Garbage Collection**: Pruning old/unused artifacts and versions
- **Integrity Checking**: Verifying artifact checksums on read
- **HTTP Artifact Server**: Serving stored artifacts over HTTP
- **Health Checking**: Reporting storage subsystem health
- **Prometheus Metrics**: Exposing storage-related metrics for monitoring

## Architecture

```
┌─────────────────────────────────────────────────┐
│                  Storage Crate                   │
├─────────────────────────────────────────────────┤
│                                                  │
│  ┌──────────────┐    ┌──────────────────────┐   │
│  │  Store       │    │  Artifact Server     │   │
│  │  (main API)  │    │  (HTTP endpoint)     │   │
│  └──────┬───────┘    └──────────────────────┘   │
│         │                                        │
│  ┌──────▼───────────────────────────────────┐   │
│  │           redb Database                  │   │
│  │  ┌──────────┐ ┌──────────┐ ┌─────────┐  │   │
│  │  │ Tables:  │ │ Tables:  │ │ Tables: │  │   │
│  │  │ Artifacts│ │ Configs  │ │ Billing │  │   │
│  │  │ Raw Wasm │ │ Secrets  │ │ Routes  │  │   │
│  │  │ KEK      │ │ Metrics  │ │ API Keys│  │   │
│  │  └──────────┘ └──────────┘ └─────────┘  │   │
│  └──────────────────────────────────────────┘   │
│                                                  │
│  ┌────────────────┐  ┌─────────────────────┐    │
│  │ Schema Migration│  │ GC / Pruning       │    │
│  └────────────────┘  └─────────────────────┘    │
│                                                  │
│  ┌────────────────┐  ┌─────────────────────┐    │
│  │ Integrity Check │  │ Health / Metrics   │    │
│  └────────────────┘  └─────────────────────┘    │
│                                                  │
└─────────────────────────────────────────────────┘
```

### Data Flow

1. **Writes**: Artifacts and data are serialized, optionally encrypted, and written to redb tables with integrity checksums
2. **Reads**: Data is read from redb, integrity-verified, deserialized, and returned
3. **GC**: Periodic pruning removes old versions and orphaned raw Wasm artifacts
4. **Migration**: On startup, schema version is checked and migrations are applied if needed

## Public API

### Core Types

| Type | Description |
|------|-------------|
| `Store` | Main storage interface; wraps the redb database |
| `ArtifactId` | Identifier for stored Wasm artifacts |
| `BillingRecord` | A single billing event record |
| `SecretEntry` | An encrypted secret stored in the database |

### Key Functions & Methods

| Function/Method | Description |
|-----------------|-------------|
| `Store::open(path)` | Open or create a database at the given path |
| `Store::store_artifact()` | Store a compiled Wasm artifact |
| `Store::get_artifact()` | Retrieve a compiled Wasm artifact |
| `Store::store_raw_wasm()` | Store a raw (uncompiled) Wasm binary |
| `Store::get_raw_wasm()` | Retrieve a raw Wasm binary |
| `Store::delete_raw_wasm_by_key()` | Delete a raw Wasm binary by key |
| `Store::store_config()` | Store an application configuration |
| `Store::get_config()` | Retrieve an application configuration |
| `Store::store_secret()` | Store an encrypted secret |
| `Store::get_secret()` | Retrieve an encrypted secret |
| `Store::store_kek()` | Store the key encryption key |
| `Store::get_kek()` | Retrieve the key encryption key |
| `Store::has_active_instances()` | Check if any artifacts have active instances |
| `Store::prune_old_versions()` | Remove old versions of artifacts |
| `Store::prune_raw_wasm_older_than()` | Remove raw Wasm older than a given duration |
| `Store::count_artifacts()` | Count the number of stored artifacts |
| `Store::artifact_exists()` | Check if an artifact exists |
| `Store::get_billing_sequence()` | Get the current billing sequence number |
| `Store::query_billing_records()` | Query billing records |
| `Store::db_path()` | Get the database path |
| `get_db_path()` | Get the database path (module-level function) |

### HTTP Artifact Server

The crate includes an HTTP server for serving stored artifacts, enabling remote retrieval of Wasm modules and binaries.

### Health Checking

Implements health check endpoints to report on storage subsystem status.

### Prometheus Metrics

Exposes metrics including:
- Total artifacts stored
- Database size
- GC statistics
- Read/write operation counts

## Known Issues & Improvements

### Critical Bugs

| Issue | Description | Impact |
|-------|-------------|--------|
| `get_db_path()` returns placeholder | Returns a hardcoded placeholder string instead of the actual database path | Breaks disk monitoring and path-dependent operations |
| `get_billing_sequence()` returns wrong value | Returns the max node number instead of the max sequence number | Billing sequence tracking is incorrect |
| `delete_raw_wasm_by_key` uses wrong key format | Key format mismatch means raw Wasm artifacts are never cleaned by GC | Disk space leaks over time |
| `has_active_instances` always returns `false` | No artifacts are protected from garbage collection | Active artifacts may be prematurely deleted |
| `prune_raw_wasm_older_than` ignores hours parameter | Deletes all raw Wasm regardless of age | Data loss of potentially needed artifacts |
| `count_artifacts` always returns `0` | Artifact count metric is non-functional | Monitoring and alerting broken |

### Versioning & Sorting Issues

| Issue | Description | Impact |
|-------|-------------|--------|
| `prune_old_versions` uses lexicographic sort | Versions like `v1`, `v10`, `v2` sort incorrectly (`v10` before `v2`) | Wrong versions may be pruned, keeping older instead of newer |

### Design & Encapsulation Issues

| Issue | Description | Impact |
|-------|-------------|--------|
| `Store::db` is `pub` | Internal database handle is publicly accessible | Breaks encapsulation; callers can bypass Store API |
| Mixed error types | Some methods return `redb::Error`, others return `PlatformError` | Inconsistent error handling for callers |
| Duplicate `db_path()`/`get_db_path()` | Two methods with same purpose but different behavior | Confusion and potential bugs |
| `panic!()` on schema downgrade | Schema version downgrade causes a panic instead of returning `Err` | Node crashes instead of graceful error handling |

### Performance Issues

| Issue | Description | Impact |
|-------|-------------|--------|
| `artifact_exists` loads entire artifact | Checks existence by loading the full artifact into memory | Wasteful memory usage; should use key existence check |
| Billing queries do full table scan | Loads and deserializes all billing records to answer queries | Poor performance at scale |

### Input Validation Issues

| Issue | Description | Impact |
|-------|-------------|--------|
| No validation on SHA-256 path parameter | Artifact server accepts unvalidated SHA-256 hashes | Potential path traversal or injection attacks |

## Security Considerations

### Critical

| Issue | Severity | Description |
|-------|----------|-------------|
| KEK stored as plaintext | **High** | The Key Encryption Key is stored as plaintext in the database. If the database is compromised, all encrypted secrets can be decrypted. The KEK should be stored outside the database (e.g., in a hardware security module or external secret store). |
| Artifact server has no authentication | **High** | The HTTP artifact server serves artifacts without any authentication or authorization. Any network-reachable client can download all stored artifacts, potentially exposing proprietary code or sensitive logic. |
| No input validation on SHA-256 path parameter | **Medium** | The artifact server endpoint that accepts SHA-256 hashes does not validate the input format. Malformed input could cause unexpected behavior or be used for path traversal attacks. |

### Recommendations

1. **KEK Protection**: Move KEK storage out of the database. Use a dedicated secret management solution (HashiCorp Vault, AWS KMS, etc.) or at minimum encrypt the KEK itself with a key derived from a node-specific secret.

2. **Artifact Server Authentication**: Add authentication middleware to the artifact server. At minimum, implement token-based or mutual TLS authentication. Consider adding authorization to restrict which artifacts each caller can access.

3. **Input Validation**: Validate all user-supplied inputs, especially the SHA-256 hash path parameter. Ensure it matches the expected format (64 hexadecimal characters) before using it in database lookups.

4. **Encryption at Rest**: Consider enabling redb's encryption-at-rest feature if available, or use filesystem-level encryption for the database file.

5. **Access Logging**: Add audit logging for all read/write operations on sensitive data (secrets, KEK, API keys).

6. **Principle of Least Privilege**: Make `Store::db` private and ensure all database access goes through the `Store` API, allowing validation and auditing at a single point.

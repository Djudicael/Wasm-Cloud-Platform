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
| `Store::persist_deployment()` | Atomically publish compiled artifact, application config and artifact hash, and clear the exact undeploy marker |
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
| `Store::begin_lifecycle_event()` | Persist a per-app broker-ordering fence; allow pending retries and reject older/completed deliveries; atomically fence admissions for removals |
| `Store::complete_lifecycle_event()` | Mark the current matching deployment/removal delivery complete |
| `get_db_path()` | Get the database path (module-level function) |

### Atomic deployment publication and undeploy cleanup

`persist_deployment(&AppId, &[u8], &AppConfig, &str)` validates matching
application identity and trigger configuration, then writes `ARTIFACTS`,
`CONFIGS` and `ARTIFACT_HASHES` and clears `_undeploy:<fullAppId>` in one redb
write transaction. A concurrent undeploy purge sees either the old fenced
state or the complete replacement deployment, not a partial publication.

Undeployed-app GC rechecks the selected marker value and its elapsed grace
period inside its write transaction. A changed marker, redeployment or
not-yet-expired grace skips the candidate. It removes the exact full application
ID from compiled artifacts, configs and artifact hashes; metrics with that
application prefix, routes targeting that exact app and its schedule cursors
are removed in the same transaction. The undeploy marker is removed only with
the successful commit. An error rolls the transaction back.

Shared content-addressed raw Wasm and verification records, secrets, API keys,
gateway configuration and lifecycle ordering fences remain intact. This
undeployed-app purge is separate from the raw/pruning APIs and their limitations
listed below. Node-level admission, shutdown and external resources are not
made transactional by this storage operation.

### HTTP Artifact Server

Lifecycle cursors use schema metadata without changing the schema version or
existing event payloads. They are scoped to stream name, stream creation epoch
and exact app ID and survive artifact GC, so delayed deployment redeliveries
cannot resurrect a removed revision after restart. They are independent of
periodic schedule cursors, whose GC lifecycle remains tied to the application.

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

### Garbage collection

- `prune_raw_wasm_older_than(hours)` currently ignores `hours` and deletes every raw artifact entry. Callers must not treat it as an age-based retention control.
- `gc_artifacts()` deletes raw Wasm by the application-version artifact key even though the raw table is keyed by SHA-256. The compiled artifact and config are removed, but the corresponding raw blob may remain.
- The private `has_active_instances()` helper uses the presence of a deployed configuration as a conservative proxy for activity. It protects deployed versions but does not observe the supervisor's live instance set.

### API and performance

- Both `db_path()` and `get_db_path()` expose the recorded database path. The duplicate names should be consolidated.
- `Store::db()` exposes the underlying redb handle to workspace callers, so those callers can bypass higher-level validation.
- Billing queries scan and deserialize the billing table. Retention and query cost must be monitored as the table grows.
- `artifact_exists()` loads the stored artifact rather than using a key-only existence check.

### Schema behavior

Schema version 8 is current. Opening a database written by a newer binary returns a storage error; it no longer panics. Supported older schemas are backed up and migrated sequentially during `Store::open()`.

## Security Considerations

- **Seal the key-encryption key.** The storage crate persists opaque KEK bytes. Node startup seals that material with the configured file, command, Vault KV, Vault Transit, AWS KMS HMAC, or development-only passphrase source. Production admission rejects the insecure unsealed path.
- **Artifact authorization is enforced for remote peers.** Loopback access is trusted. Non-loopback reads require a valid signed transfer manifest; writes require a compatible bearer token or signed manifest. Operators must still protect the listener with the platform TLS/network policy.
- **Raw artifacts are content-addressed.** PUT computes SHA-256 and rejects a mismatch. GET uses the supplied value only as a redb key; it is not interpolated into a filesystem path.
- **Protect the database file.** redb does not provide application-level encryption for every table. Use restrictive filesystem permissions and encrypted storage where the threat model requires it.
- **Treat storage access as privileged.** Secrets, management state, artifact credentials, and billing records share the database. Limit direct use of `Store::db()` and audit sensitive mutations.

# Billing

Audit-grade billing and metering for the Wasm Cloud Platform.

## Overview

The `billing` crate provides audit-grade billing and metering infrastructure. It collects per-invocation data via an async channel, batches writes to redb, and chains records into a tamper-evident hash chain (SHA-256). The crate supports chain integrity verification, tenant billing report generation, and export to S3 or local files.

## Architecture

The billing system operates as a pipeline:

1. **Collection** — `BillingCollector` receives `BillingInput` records through an async channel, decoupling producers from the persistence layer.
2. **Persistence** — A background writer batches records and writes them to redb, chaining each record's hash to the previous record (SHA-256) to form a tamper-evident log.
3. **Verification** — `verify_chain` walks the stored records and validates that every `prev_hash` matches the hash of the preceding record, detecting any tampering or corruption.
4. **Reporting** — `generate_report` and `generate_tenant_billing_report` aggregate usage data per tenant and application.
5. **Export** — `BillingExporter` implementations (`S3Exporter`, `FileExporter`) write reports to external destinations. `start_export_loop` runs a periodic export cycle.

Key storage concepts:
- Each record has a monotonically increasing sequence number and a `prev_hash` linking it to the prior record.
- Records are stored in redb with per-node sequencing.
- `TenantCache` provides cached access to tenant metadata.

## Public API

| Type | Description |
|------|-------------|
| `BillingCollector` | Entry point for recording billing events; sends `BillingInput` through an async channel |
| `BillingInput` | Input data for a single billable invocation |
| `BillingRecord` | Persisted billing record with sequence number, hash chain link, and invocation metadata |
| `verify_chain` | Verifies integrity of the hash chain across stored records |
| `ChainError` | Errors that can occur during chain verification |
| `generate_report` | Generates a billing report for a given time range |
| `TenantBillingReport` | Aggregated billing report for a single tenant |
| `AppUsage` | Per-application usage statistics within a tenant report |
| `BillingExporter` | Trait for exporting billing reports to external destinations |
| `S3Exporter` | Exports reports to an S3-compatible object store |
| `FileExporter` | Exports reports to local files |
| `start_export_loop` | Starts a background loop that periodically exports billing data |
| `TenantCache` | Cached lookup for tenant metadata |

## Known Issues & Improvements

### Data Integrity

- **Hash chain breaks on restart** — `flush_batch` does NOT persist `seq` or `prev_hash`. After a restart, the chain cannot be continued correctly, breaking tamper-evidence guarantees.
- **Wrong sequence semantics** — `get_billing_sequence` in storage returns the max node number instead of the max sequence, leading to incorrect sequence assignment.
- **Cross-node hash lookup** — `get_last_billing_hash` finds the global max sequence, but sequences are per-node. This returns the wrong hash when multiple nodes exist.
- **Non-transactional writes** — `flush_batch` writes records individually rather than in a single redb transaction. A partial failure leaves the store in an inconsistent state.

### S3 Export

- **Placeholder SigV4 signature** — `S3Exporter` uses a placeholder AWS SigV4 signature implementation. Requests to real S3 endpoints will be rejected with authentication errors.
- **HTTP 307 treated as success** — The exporter treats a 307 redirect as a successful upload. Data is silently lost because the redirect is not followed.
- **New HTTP client per call** — `S3Exporter` creates a new `reqwest::Client` on every export call, incurring unnecessary overhead and connection setup cost.
- **No HTTPS validation** — There is no validation that the S3 endpoint URL uses HTTPS, allowing credentials and data to be sent over plaintext connections.

### Runtime & Reliability

- **Blocking async runtime** — Blocking store methods are called from async contexts, which blocks the Tokio runtime and can cause stalls across the system.
- **Silent record drops** — `BillingCollector::record` silently drops records when the async channel is full, leading to unreported usage and billing gaps.
- **Unbounded memory usage** — `generate_tenant_billing_report` and `get_tenant_list` load ALL records into memory. For large deployments, this can cause out-of-memory conditions.
- **No retry logic** — Failed exports have no retry, backoff, or dead-letter mechanism. Transient failures cause permanent data loss.
- **No graceful shutdown** — There is no graceful shutdown for the export loop or billing writer. In-flight records may be lost on termination.

### Error Handling

- **ChainError incomplete** — `ChainError` doesn't implement `std::error::Error` or `Display`, making it incompatible with the broader Rust error ecosystem.
- **Inconsistent error types** — The crate uses `ChainError`, `String`, and `PlatformError` inconsistently across different functions, making error handling unpredictable.

## Security Considerations

- **S3 credentials in plaintext** — S3 credentials are stored as plain `Option<String>` with no `Zeroize` or `Secret` wrapper. Credentials may remain in memory indefinitely and could be exposed through memory dumps or debug logs.
- **No HTTPS enforcement** — The S3 exporter does not validate that the endpoint URL uses HTTPS, allowing credentials and billing data to be transmitted over unencrypted connections.
- **Tamper-evidence is incomplete** — The hash chain is broken on restart (see above), which means the tamper-evidence guarantee does not survive process restarts. An attacker with write access to redb could modify records after a restart without detection.
- **No access control** — There is no authorization layer controlling who can read billing records, generate reports, or modify the chain. Any code with access to the store can alter billing data.
- **No audit logging** — Secret access and billing record modifications are not logged, making it difficult to detect unauthorized access or investigate incidents.

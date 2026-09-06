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

- **Record and cursor writes are not atomic** — the writer recovers a per-node `(seq, prev_hash)` cursor after restart, but each record and the cursor use separate redb transactions. A write failure or crash between these operations can leave gaps or a cursor that does not describe the durable tail.
- **A batch is not one storage transaction** — `flush_batch` writes records individually. Its batching reduces channel and logging overhead, but not redb commits, and a partial failure can persist only part of the batch.
- **Global helpers are ambiguous for multi-node data** — the collector correctly uses `get_billing_sequence_for_node` and `get_last_billing_hash_for_node`. The older global helpers should not be used to resume a node-local chain.

### S3 Export

- **Placeholder SigV4 signature** — `S3Exporter` uses a placeholder AWS SigV4 signature implementation. Requests to real S3 endpoints will be rejected with authentication errors.
- **No HTTPS requirement** — endpoint construction accepts HTTP as well as HTTPS, so deployment configuration must prevent plaintext credential and report transport.
- **No export-loop shutdown handle** — `start_export_loop` detaches its task and does not return a cancellation or join handle.

### Runtime & Reliability

- **Blocking async runtime** — Blocking store methods are called from async contexts, which blocks the Tokio runtime and can cause stalls across the system.
- **Lossy overload behavior** — `BillingCollector::record` drops records when its bounded channel is full. It increments an internal counter and warns on the first and every thousandth drop, but the counter is not exposed through its public API.
- **Unbounded memory usage** — `generate_tenant_billing_report` and `get_tenant_list` load ALL records into memory. For large deployments, this can cause out-of-memory conditions.
- **Fixed-interval export retry** — a failed export leaves the watermark unchanged and is retried on the next interval, without backoff, jitter, or a dead-letter path.
- **Shutdown completion cannot be awaited** — `BillingCollector::shutdown` asks the writer to flush buffered records, but it does not return the writer's join handle. The detached export loop has no shutdown API.

### Error Handling

- **Inconsistent error types** — `ChainError` implements `Display` and `std::error::Error`, while collector report helpers still return `String` and exporters return `PlatformError`.

## Security Considerations

- **S3 credentials in plaintext** — S3 credentials are stored as plain `Option<String>` with no `Zeroize` or `Secret` wrapper. Credentials may remain in memory indefinitely and could be exposed through memory dumps or debug logs.
- **No HTTPS enforcement** — The S3 exporter does not validate that the endpoint URL uses HTTPS, allowing credentials and billing data to be transmitted over unencrypted connections.
- **Tamper evidence depends on cursor consistency** — per-node cursors let chains continue across ordinary restarts, but record writes and cursor persistence are not one transaction. Verification detects modified links; it does not prevent an actor with database write access from replacing a complete chain and its cursor.
- **No access control** — There is no authorization layer controlling who can read billing records, generate reports, or modify the chain. Any code with access to the store can alter billing data.
- **No audit logging** — Secret access and billing record modifications are not logged, making it difficult to detect unauthorized access or investigate incidents.

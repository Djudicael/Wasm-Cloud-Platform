# P10-03 telemetry remediation evidence

Validation date: 2026-08-30  
Scope: three-node Firecracker platform, HAProxy, PostgreSQL-backed OIDC Hub,
state-scoped Podman telemetry stack  
Result: **LOCAL PASS WITH AN EXPLICIT COLLECTOR-OUTAGE DURABILITY BOUNDARY**

## Proven

- `logging.otlp_endpoint` now activates the node's combined structured logging
  and OpenTelemetry subscriber without an initialization panic.
- Static-IP microVM boots receive the persisted OTLP endpoint through
  `wcp.otlp_endpoint`; node-image schema 9 rejects older cached images.
- W3C context is extracted at the platform proxy and injected upstream.
- Tempo trace `eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee` is queryable with:
  `service.name=wasm-node`, `service.version=0.1.0`,
  `service.instance.id=local-test-node-0`,
  `deployment.environment=local-validation`, the exact OIDC deployment,
  `/health/ready`, and HTTP `200`.
- The OIDC application preserves the propagated ID. Earlier trace
  `99999999999999999999999999999999` correlated the proxy completion to a WASI
  TCP connection at `172.20.0.20:5432`, PostgreSQL 17.11 authentication, and
  `SELECT 1`.
- Operational and audit exports are distinct mode-0600 files. The audit export
  contained audit envelopes; the operational export contained zero audit-target
  records. Sensitive audit details remain only in the authoritative node file.
- With Tempo stopped, the Collector's disk-backed queue rose to six batches out
  of 2048, its storage file was present, OIDC stayed ready, and
  `TraceBackendDown` fired. After restart, trace
  `55555555555555555555555555555555` was queryable and the queue drained.
- With the Collector stopped, `TelemetryCollectorDown` fired and OIDC remained
  available. Export resumed after restart and filelog state remained on disk.
- After final rolling replacement, all three nodes accepted traffic with two
  healthy OIDC components, all 11 Prometheus targets were up, the public
  readiness response reported `database=ok`, and active alerts resolved to zero.

Each node's aggregate admin status remained `degraded` solely because the
2-GiB local rootfs had about 1.5 GiB free and triggered its conservative disk
headroom policy. Backpressure was healthy and requests were accepted. This is
the separately tracked P10-08 production sizing issue, not a telemetry outage.

## Important boundary

Trace `77777777777777777777777777777777`, created while the immediate Collector
was stopped, was not recovered. The node batch exporter is bounded in memory but
is not durable. Disk durability begins in the Collector's exporter queue.
Production must run a supervised local Collector per host or implement a
node-side WAL before claiming lossless Collector interruption.

## Findings fixed during the run

1. MMDS OTLP configuration was ineffective for static-IP guests because their
   init branch bypassed MMDS. The endpoint is now a Firecracker kernel argument
   and environment override.
2. The first Tempo configuration reused Prometheus port 9095 for Tempo's control
   gRPC listener. Tempo now uses a distinct internal port.
3. Filelog JSON parsing promoted `target` to attributes, so the initial filters
   duplicated audit records. Filters now use `attributes["target"]`.
4. An immediate drain followed by replacement can leave a replacement fenced.
   The runbook now requires waiting for drain completion/removal before restart.
5. Trace resources initially lacked node identity. The final image emits
   `service.instance.id`; the local Collector adds the environment attribute.

## Verification commands and results

- `cargo fmt --all -- --check`: pass.
- `cargo clippy --workspace --all-targets -- -D warnings`: pass.
- Native workspace check with both WASI examples excluded: pass.
- `http-hello-component` and `wasi-grpc-echo` `wasm32-wasip2` builds: pass.
- Focused metrics/proxy/supervisor/vm-testbed tests: 175 passed.
- Common/config tests: 137 unit tests plus one doc test passed.
- Node binary tests: 78 passed.
- OIDC trace middleware tests: 3 passed.
- Modified shell scripts passed `bash -n`.

The existing `proc-macro-error2 2.0.1` future-incompatibility notice was still
printed. It is tracked separately and was not introduced by P10-03.

No credentials, bearer tokens, database URLs, cookies, or audit detail payloads
are included in this evidence record.

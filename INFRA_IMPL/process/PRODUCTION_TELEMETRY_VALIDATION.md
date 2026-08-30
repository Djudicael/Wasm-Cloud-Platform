# Production telemetry validation

This runbook defines the platform telemetry contract and the tests required
before a production promotion. PostgreSQL, Tempo, Prometheus, and log storage
are integration targets; the platform does not own or certify their production
high availability.

## Required production shape

Each platform host must run a supervised local telemetry agent. Nodes send OTLP
to that local agent, and journald or serial logs are tailed locally. The agent
must use a bounded disk-backed queue before forwarding to independently operated
trace, log, and audit backends.

```text
Wasm node -- OTLP/W3C --> local Collector -- durable queue --> trace backend
    |                         |
    +-- JSON stdout/journal --+-- operational log stream
                              +-- restricted audit stream
```

Do not send every node directly to a remote central Collector unless loss during
network or Collector interruption is explicitly accepted. The Rust batch span
processor is bounded in memory but is not a write-ahead log. The local rehearsal
proved durable recovery from a trace-backend outage and also proved that a span
can be lost when the immediate Collector itself is stopped.

Production controls:

- supervise and restart the local agent independently of `wasm-node`;
- place its queue on a sized, monitored Linux filesystem;
- define queue capacity, retention, retry deadline, and drop policy;
- use authenticated TLS for OTLP and backend export;
- isolate audit credentials, storage, access, and retention from operational logs;
- forward audit data to append-only or immutable off-host storage;
- alert on agent/backend availability, queue utilization, enqueue failures,
  send failures, refused records, and filesystem pressure;
- never put bearer tokens, cookies, passwords, connection strings, private keys,
  or complete request bodies in logs, spans, attributes, or baggage.

## Canonical identity fields

Platform traces and request-completion logs must expose:

- `deployment.environment`;
- `service.name` and `service.version`;
- `service.instance.id` (the platform node ID);
- W3C trace ID and span ID;
- application/deployment ID;
- HTTP method, route/path, status, and latency;
- timestamp, severity, component/target, and node ID in structured logs.

Applications must preserve inbound `traceparent` or the platform-provided
`X-Trace-Id`. Database libraries should execute inside the request span or emit
the trace ID explicitly. The OIDC reference application implements this contract,
which lets a readiness request correlate with its WASI TCP connection,
PostgreSQL authentication, and `SELECT 1` event.

Do not trust arbitrary correlation headers as log content. Validate their size
and hexadecimal format, reject all-zero IDs, and generate a local ID when the
context is invalid.

## Local microVM rehearsal

Run in WSL2 and keep one state file throughout:

```bash
export CARGO_TARGET_DIR=/tmp/wasm-cloud-platform-target

bash scripts/vm/provision-testbed.sh \
  --preset production-like \
  --nodes 3 \
  --front-door haproxy \
  --front-door-bind 127.0.0.1:8088 \
  --node-otlp-endpoint http://172.20.0.1:4317 \
  --state-file .prod-validation-single-host-state.json

bash scripts/vm/provision-observability.sh \
  --state-file .prod-validation-single-host-state.json
```

The node image schema is checked before provisioning. Static-IP guests receive
the OTLP endpoint through the Firecracker kernel command line because that boot
path does not execute the MMDS network setup branch.

Deploy the PostgreSQL-backed OIDC example with the canonical deployment script.
It runs migrations, deploys both WASI components, creates the same-origin HAProxy
split, and verifies database readiness.

## Acceptance checks

### 1. Trace propagation

Send a known valid trace context through HAProxy:

```bash
curl -fsS \
  -H 'Host: oidc-backend.internal' \
  -H 'traceparent: 00-11111111111111111111111111111111-2222222222222222-01' \
  http://127.0.0.1:8088/health/ready

curl -fsS \
  http://127.0.0.1:3200/api/traces/11111111111111111111111111111111
```

Pass only when the trace is queryable and reports the expected node instance,
environment, exact OIDC deployment, path, status `200`, and completion event.
The application log must show the same trace ID around the PostgreSQL connection
and query.

### 2. Operational and audit separation

Generate one authenticated admin action. Confirm the dedicated audit export has
the minimal audit envelope and the operational export has no `target=audit`
record. Both local files must be protected; production destinations must also
have distinct credentials and authorization policies.

The authoritative node audit file retains sensitive details. The collected
envelope deliberately omits the free-form detail field.

### 3. Trace-backend interruption

Resolve the Tempo container ID from the companion service state and stop only
that exact container. Generate requests and verify:

- OIDC remains ready;
- `TraceBackendDown` fires;
- Collector queue capacity is fixed and queue size rises;
- the disk storage file exists and remains bounded;
- enqueue failures remain zero at the tested load.

Restart that same container. Pass when the queue drains, the trace becomes
queryable, and the alert resolves.

### 4. Collector interruption

Stop only the recorded Collector container, generate a request, and verify the
application remains available and `TelemetryCollectorDown` fires. Restart it
and verify new telemetry resumes and file-log offsets persist.

Record whether the outage span is recovered. With the current node exporter it
is not guaranteed. A production design must therefore keep the local agent
highly supervised or add a node-side WAL before claiming Collector-outage
durability.

### 5. Drop and saturation behavior

Load must be bounded and deliberate. Observe queue capacity, queue size, enqueue
failures, send failures, refused spans/logs, Collector RSS, disk use, and
application readiness. Define alert thresholds below exhaustion and demonstrate
both firing and resolution. Never fill the host filesystem to prove the limit.

### 6. Restart behavior

For a rolling replacement, publish a targeted drain, wait for the advertised
drain period and readiness removal, and only then replace the process. The local
test found that an immediate drain/restart can let the replacement consume or
retain the terminal fence and start with `accepting_requests=false`. Verify every
replacement has reconstructed deployments before advancing to the next node.

After replacing a VM, recreate the disposable Collector container so its exact
serial-log bind mount follows the new file inode. A production journald/socket
receiver does not have this test-fixture limitation.

## Final gate

Pass the local gate only when all nodes accept traffic, every intended
Prometheus target is up, no unexpected alert remains, audit data is separated,
one PostgreSQL-backed OIDC trace is queryable, and the interruption results are
recorded without overstating durability.

The local result does not certify production PKI, immutable retention, backend
HA, multi-host failure, provider networking, or production throughput. Those
belong to the two-host and provider staging plans.

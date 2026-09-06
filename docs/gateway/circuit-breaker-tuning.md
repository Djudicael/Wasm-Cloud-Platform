# Circuit Breaker Tuning Guide

This guide explains how to configure and tune the circuit breaker for your Wasm applications.

## What is a Circuit Breaker?

A circuit breaker prevents cascading failures by stopping requests to an unhealthy upstream app. It has three states:

```
┌─────────┐   failures >= threshold   ┌─────────┐   reset timeout   ┌───────────┐
│  CLOSED │ ─────────────────────────→│  OPEN   │ ────────────────→│ HALF-OPEN │
│ (normal)│                           │(reject) │   expires        │  (probe)  │
└─────────┘←──────────────────────────┴─────────┴──────────────────┴─────┬─────┘
     ↑                                                                    │
     └──────────────── probe succeeds ────────────────────────────────────┘
                         (back to CLOSED)

     └──────────────── probe fails ─────────────────────────────────────→
                         (back to OPEN)
```

- **Closed**: Requests flow normally. Failures are counted.
- **Open**: All requests are rejected immediately with `503 Service Unavailable`.
- **Half-Open**: One probe request is allowed through to test recovery.

## When to Use a Circuit Breaker

Use a circuit breaker when:

- Your app occasionally crashes or returns 500 errors
- You want to prevent one failing app from overwhelming the proxy
- You want fast failure instead of slow timeouts for unhealthy apps

Don't use a circuit breaker when:

- Your app is completely stateless and restarts instantly (the cold start handles it)
- Your app returns 4xx errors as part of normal operation (these don't count as failures by default)

## Configuration Format

```toml
[app.gateway.circuit_breaker]
failure_threshold = 5
reset_timeout_secs = 30
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `failure_threshold` | No | `5` | Consecutive failures before opening the circuit |
| `reset_timeout_secs` | No | `30` | Seconds to wait before allowing a probe request |

## Default Behavior

Routes without explicit circuit breaker config use the node's defaults:

```toml
[gateway.circuit_breaker]
default_failure_threshold = 5
default_reset_timeout_secs = 30
```

## Tuning Examples

### Strict (Low Tolerance)

For critical services where any failure is significant:

```toml
[app.gateway.circuit_breaker]
failure_threshold = 2
reset_timeout_secs = 60
```

- Circuit opens after **2 failures** (very sensitive)
- Waits **60 seconds** before probing (longer recovery time, fewer false positives)

**Use case**: Payment processing, authentication services

### Lenient (High Tolerance)

For non-critical services that occasionally hiccup:

```toml
[app.gateway.circuit_breaker]
failure_threshold = 10
reset_timeout_secs = 10
```

- Circuit opens after **10 failures** (tolerates brief issues)
- Waits only **10 seconds** before probing (fast recovery)

**Use case**: Analytics, logging, background processing

### Standard (Balanced)

For general-purpose APIs:

```toml
[app.gateway.circuit_breaker]
failure_threshold = 5
reset_timeout_secs = 30
```

- Circuit opens after **5 failures**
- Waits **30 seconds** before probing

**Use case**: Most REST APIs, CRUD services

### Aggressive Recovery

For services that recover quickly (e.g., cold-start apps):

```toml
[app.gateway.circuit_breaker]
failure_threshold = 3
reset_timeout_secs = 5
```

- Circuit opens after **3 failures**
- Probes after only **5 seconds**

**Use case**: Development environments, auto-scaling services

## What Counts as a Failure?

By default, the circuit breaker counts:

- **5xx HTTP responses** (500, 502, 503, etc.)
- **Connection errors** (upstream unreachable, connection refused, timeout)

It does **not** count:

- **4xx responses** (404, 422, etc.) — these are client errors
- **Successful responses** (200–299)
- **Redirects** (300–399)

## Monitoring Circuit Breaker State

### Prometheus Metrics

| Metric | Description |
|--------|-------------|
| `wasm_gateway_circuit_breaker_rejected_total` | Total requests rejected by circuit breaker |
| `wasm_gateway_circuits_open` | Number of currently open circuits |

### Admin API

Check the gateway config for an app:

```bash
curl http://localhost:9090/admin/gateway/my-app:v1
```

### Logs

The gateway logs state transitions:

```
WARN  circuit breaker: CLOSED → OPEN (too many failures) app=my-app:v1 failures=5
INFO  circuit breaker: OPEN → HALF-OPEN app=my-app:v1
INFO  circuit breaker: HALF-OPEN → CLOSED (recovered) app=my-app:v1
WARN  circuit breaker: HALF-OPEN → OPEN (probe failed) app=my-app:v1
```

## CLI Commands

```bash
# Set circuit breaker config
wasm-ctl gateway set-circuit-breaker my-app:v1 \
  --failure-threshold 5 \
  --reset-timeout 30

# View current config
wasm-ctl gateway show my-app:v1

# Remove circuit breaker config (revert to default)
wasm-ctl gateway reset my-app:v1
```

## Testing Circuit Breaker Behavior

### Simulate failures

Deploy an app that returns 500 errors, then send requests:

```bash
# Send 10 requests — after 5, the circuit should open
for i in {1..10}; do
  curl -s -o /dev/null -w "%{http_code}\n" http://localhost:8080/
done
```

Expected output:

```
500
500
500
500
500
503  # circuit opened
503
503
503
503
```

### Verify recovery

Wait 30 seconds (or your `reset_timeout_secs`), then send one request:

```bash
sleep 30
curl -s -o /dev/null -w "%{http_code}\n" http://localhost:8080/
```

If the app is healthy: `200` (circuit closes)
If the app is still failing: `503` (circuit re-opens)

## Common Issues

### Circuit opens too quickly

**Symptom**: Circuit opens after only 1–2 errors

**Causes**:
- `failure_threshold` is too low
- The app has a genuine issue (check logs)
- A deployment is in progress (temporary)

**Fix**:
```toml
# Increase threshold if the app occasionally hiccups
failure_threshold = 10
```

### Circuit never recovers

**Symptom**: Circuit stays open indefinitely

**Causes**:
- The app is permanently broken
- `reset_timeout_secs` is very high
- The probe request keeps failing

**Fix**:
```bash
# Check running instances, then correlate with node logs and metrics
wasm-ctl instances

# Fix the app (redeploy, check logs)
wasm-ctl deploy --app my-app --version v2 --wasm fixed.wasm

# Or reduce reset timeout for faster recovery
wasm-ctl gateway set-circuit-breaker my-app:v1 --reset-timeout 10
```

### False positives during deploy

**Symptom**: Circuit opens during rolling deploys

**Cause**: Old instances are killed before new instances are fully ready

**Fix**:
- Ensure health checks pass before marking instances ready
- Increase `failure_threshold` temporarily during deploys
- wait for the new version to become healthy before switching its route

## Best Practices

1. **Start with defaults** (threshold=5, timeout=30) and adjust based on observation
2. **Set thresholds based on SLOs**: If your SLO is 99.9% uptime, a threshold of 3 may be too aggressive
3. **Monitor `wasm_gateway_circuits_open`**: Alert if circuits are open for >5 minutes
4. **Use different settings per environment**:
   - Development: threshold=10, timeout=10 (forgive instability)
   - Production: threshold=5, timeout=30 (strict but reasonable)
5. **Don't set threshold = 1**: This causes the circuit to open on any single error
6. **Don't set timeout = 0**: This causes rapid oscillation between open and half-open

## Integration with Other Gateway Features

The circuit breaker works alongside other gateway features:

```toml
[app.gateway.auth]
policy = "authenticated"

[app.gateway.rate_limit]
requests_per_second = 1000

[app.gateway.circuit_breaker]
failure_threshold = 5
reset_timeout_secs = 30
```

Request flow when circuit is open:

1. Authentication runs first → 401 if no token
2. Rate limiting runs next → 429 if over limit
3. **Circuit breaker** → **503** if circuit is open (skips upstream entirely)

This means an unauthenticated request to an app with an open circuit still gets 401, not 503. The circuit breaker only protects requests that have passed earlier checks.

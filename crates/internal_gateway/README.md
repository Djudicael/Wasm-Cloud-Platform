# internal_gateway

## Overview

The `internal_gateway` crate provides a transparent internal gateway for east-west traffic between applications on the same platform node. It listens on loopback, using port 9080 by default, and routes requests to a local backend based on the `Host` header.

The gateway parses incoming `Host` headers in the format `<app>.<namespace>.internal`, resolves the target application, applies any configured policies (authentication, rate limiting, etc.), and forwards the request to the appropriate app instance.

## Architecture

```
┌──────────────┐     ┌──────────────────────┐     ┌──────────────────┐
│  Source App  │────▶│  Internal Gateway    │────▶│  Target App      │
│  (outbound)  │     │  :9080 (loopback)    │     │  (inbound)       │
└──────────────┘     │                      │     └──────────────────┘
                     │  1. Parse Host header │
                     │  2. Resolve target    │
                     │  3. Apply policies    │
                     │  4. Forward request   │
                     └──────────────────────┘
```

### Request Flow

1. **Inbound Request** — An application sends an HTTP request to `127.0.0.1:9080` with a `Host` header set to `<app>.<namespace>.internal`.
2. **Host Parsing** — `parse_internal_host()` extracts the app name and namespace from the `Host` header.
3. **Caller Attribution** — At connection acceptance, snapshot the owner from the authoritative eBPF gateway maps; revalidate its connection and registration generation for every request. Missing or unreadable identity fails closed. Ring-buffer telemetry and caller headers cannot supply an authorization fallback.
4. **Target Resolution** — The gateway looks up the target application configuration using the parsed app ID and namespace.
5. **Policy Application** — Route-level and endpoint-level policies are evaluated, including JWT auth, role checks, API keys, and endpoint scope checks.
6. **Forwarding** — The request is forwarded to the resolved target application instance using HTTP/1.1 or `h2c`, depending on the target app model.

## Public API

### `InternalGateway`

The main gateway struct that binds to the loopback interface and handles incoming connections. Responsible for orchestrating the full request lifecycle from parsing through forwarding.

### Host parsing helper

```rust
fn parse_internal_host(host: &str) -> Option<(&str, &str)>
```

This private helper parses `<app>.<namespace>.internal[:port]` and returns borrowed `(app_name, namespace)` values. A bare `<app>.internal` name uses the `default` namespace, and app names may themselves contain dots.

### `EndpointAuth`

Enum representing authentication requirements for an endpoint. Supported modes include:

- `none`
- `authenticated`
- `roles`
- `api_key`
- `inherit`

Endpoint rules may also require JWT scopes through `required_scopes`.

## Known Issues & Improvements

| Issue | Severity | Description |
|-------|----------|-------------|
| Internal gateway requires eBPF-backed identity attribution | **High** | Requests are accepted only when caller identity resolves through the live kernel gateway maps with eBPF enforcement active. If that attribution path is unavailable, the gateway fails closed. |
| Content-Length-based body cap only | **Medium** | The gateway rejects oversized requests when `Content-Length` exceeds the configured cap. Chunked uploads without a trusted length still rely on upstream behavior, so add streaming byte accounting if you need a strict on-the-wire hard stop. |

## Security Considerations

### Bounded forwarding

The supervisor registers each local endpoint with the target application's
operator-admitted `request_timeout_ms`. The gateway uses that value, or the
legacy 30-second default when it is omitted. Explicit values must be
1000..300000 ms; invalid registry metadata fails closed before a connection is
attempted. Caller headers never select or enlarge this budget.

One absolute deadline covers TCP connection, HTTP/1 or HTTP/2 handshake and
response headers, without resetting the budget between phases. TCP connect is
also capped at 30 seconds or the smaller remaining target budget. A failure or
cancellation before headers aborts the owned connection driver. After headers,
the existing streamed-body contract remains unchanged; the caller's HTTP
transport and the target HTTP Store retain their independent bounded deadlines.
The default value remains 30 seconds, while its scope is now global. Requests
that previously accumulated separate phase waits can therefore time out sooner
without an explicit override. This is a change in enforcement, not a claim of
identical timing behavior for existing deployments.

Cold start must return an address already registered by the supervisor. The
gateway re-reads the exact local endpoint, preserving its budget and protocol;
missing or mismatched registration fails closed rather than fabricating
default metadata. This is node-local only and does not introduce cross-host
service discovery. Kernel-derived identity, namespace checks, allowlists,
authentication and rate limiting remain unchanged.
Rust `RegisteredEndpoint` literals must include `request_timeout_ms`; legacy
registration supplies `None`. Application JSON remains compatible through the
optional `AppConfig` member: missing fields use `None`, which is omitted on
serialization. Endpoint budgets are internal trusted metadata, not a new
caller-visible header or guest-controlled value.

- **Service Naming Model**: Internal service discovery is intentionally app-name based inside a namespace. If you want a parallel incompatible rollout, deploy it as a separate app with a different name rather than expecting version-qualified internal hostnames.

- **Namespace Isolation**: The gateway strips forged internal identity headers, resolves and revalidates caller identity from the authoritative kernel gateway maps, and enforces cross-namespace deny-by-default with explicit allowlists. The gateway is intended to run only with eBPF-backed caller attribution and now fails closed when that identity path is unavailable.

- **Rate Limiting Coverage**: The gateway enforces source-app rate limiting and endpoint-rule rate limits on the slow path. If you need stricter tenant isolation or request-shape-aware throttling, add per source/target pair policies rather than relying only on global app-level limits.

- **Loopback-Only Binding**: The gateway binds only to the loopback interface, which limits exposure to local processes. This is a required security property and should be maintained.

- **Plaintext Internal Traffic**: Traffic between the gateway and backend apps is unencrypted and remains on loopback. The `.internal` mesh is deliberately node-local; callers must use an explicit external endpoint and its separate security policy for remote-node traffic.

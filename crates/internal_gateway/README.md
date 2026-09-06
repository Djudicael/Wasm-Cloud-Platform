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
3. **Target Resolution** — The gateway looks up the target application configuration using the parsed app ID and namespace.
4. **Policy Application** — Route-level and endpoint-level policies are evaluated, including JWT auth, role checks, API keys, and endpoint scope checks.
5. **Forwarding** — The request is forwarded to the resolved target application instance using HTTP/1.1 or `h2c`, depending on the target app model.

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
| Internal gateway requires eBPF-backed identity attribution | **High** | Requests are accepted only when caller identity resolves through the namespace map with eBPF enforcement active. If that attribution path is unavailable, the gateway fails closed. |
| Content-Length-based body cap only | **Medium** | The gateway rejects oversized requests when `Content-Length` exceeds the configured cap. Chunked uploads without a trusted length still rely on upstream behavior, so add streaming byte accounting if you need a strict on-the-wire hard stop. |

## Security Considerations

- **Service Naming Model**: Internal service discovery is intentionally app-name based inside a namespace. If you want a parallel incompatible rollout, deploy it as a separate app with a different name rather than expecting version-qualified internal hostnames.

- **Namespace Isolation**: The gateway strips forged internal identity headers, resolves caller identity from the namespace map, and enforces cross-namespace deny-by-default with explicit allowlists. The gateway is intended to run only with eBPF-backed caller attribution and now fails closed when that identity path is unavailable.

- **Rate Limiting Coverage**: The gateway enforces source-app rate limiting and endpoint-rule rate limits on the slow path. If you need stricter tenant isolation or request-shape-aware throttling, add per source/target pair policies rather than relying only on global app-level limits.

- **Loopback-Only Binding**: The gateway binds only to the loopback interface, which limits exposure to local processes. This is a required security property and should be maintained.

- **Plaintext Internal Traffic**: Traffic between the gateway and backend apps is unencrypted and remains on loopback. The `.internal` mesh is deliberately node-local; callers must use an explicit external endpoint and its separate security policy for remote-node traffic.

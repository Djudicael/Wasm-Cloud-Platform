# internal_gateway

## Overview

The `internal_gateway` crate provides a transparent internal gateway for East-West traffic between applications running on the platform. It listens on the loopback interface on port 9080 and acts as a reverse proxy that routes requests to the appropriate backend application based on the `Host` header.

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
4. **Policy Application** — Authentication and authorization policies are evaluated (currently placeholder implementations).
5. **Forwarding** — The request is forwarded to the resolved target application instance.

## Public API

### `InternalGateway`

The main gateway struct that binds to the loopback interface and handles incoming connections. Responsible for orchestrating the full request lifecycle from parsing through forwarding.

### `parse_internal_host()`

```rust
pub fn parse_internal_host(host: &str) -> Option<(String, String)>
```

Parses a `Host` header value in the format `<app>.<namespace>.internal` and returns a tuple of `(app_name, namespace)`. Returns `None` if the host does not conform to the expected format.

### `EndpointAuth`

Enum representing authentication requirements for an endpoint. Currently includes:

- `EndpointAuth::Unauthenticated` — No authentication required.
- `EndpointAuth::Authenticated` — Authentication required (placeholder, no enforcement).

### `Roles`

Represents authorization roles for access control. Currently a no-op placeholder with no enforcement logic.

## Known Issues & Improvements

| Issue | Severity | Description |
|-------|----------|-------------|
| `parse_internal_host` fails for dotted app names | **High** | App names containing dots (e.g., `my.app`) will cause incorrect parsing since dots are used as delimiters. Consider using a different delimiter or escaping scheme. |
| Request body read with `usize::MAX` limit | **Critical** | Reading the request body with no practical limit creates an out-of-memory risk. A malicious or misconfigured client could send an extremely large body. Should use a reasonable limit (e.g., 10 MB). |
| New `reqwest::Client` per request | **High** | Creating a new HTTP client for every forwarded request is extremely wasteful. Connection pooling and TLS session reuse are lost. A shared client should be created once and reused. |
| `target_app_id` missing version | **Medium** | The target application ID is constructed without a version component, which may cause config lookup failures when multiple versions of an app are deployed. |
| No namespace isolation | **High** | There is no enforcement of namespace boundaries. Any app can reach any other app in any namespace, violating tenant isolation. |
| `EndpointAuth::Authenticated` and `Roles` are no-ops | **Medium** | Authentication and authorization types exist but have no enforcement logic. All requests are effectively unauthenticated. |
| Per-endpoint rate limiting not implemented | **Medium** | Rate limiting is referenced in the design but not implemented. A single misbehaving app could overwhelm a target service. |
| No request timeout on forwarding client | **Medium** | Forwarded requests have no timeout, meaning a hung backend could indefinitely block a gateway worker thread. |
| Full body buffering before forwarding | **Medium** | The gateway buffers the entire request body before forwarding, which increases latency and memory usage. Streaming the body would be more efficient. |
| `ConnectInfo` `peer_addr` captured but unused | **Low** | The peer address is extracted from connection info but never used for logging, access control, or rate limiting. |

## Security Considerations

- **Namespace Isolation**: The gateway currently does not enforce namespace boundaries. Any application can reach any other application regardless of namespace. This is a critical isolation failure in multi-tenant deployments. Implement namespace-scoped allowlists or deny-by-default policies.

- **Authentication Bypass**: `EndpointAuth::Authenticated` and `Roles` are placeholder types with no enforcement. Until these are implemented, all internal endpoints are effectively open to any caller on the loopback interface.

- **OOM via Large Body**: The request body is read with a `usize::MAX` limit, allowing a malicious app to exhaust gateway memory. Apply a strict body size limit.

- **No Rate Limiting**: Without per-endpoint rate limiting, a compromised or buggy app can flood another app with requests. Implement token-bucket or sliding-window rate limiting per source/target pair.

- **No Request Timeout**: Forwarded requests have no timeout, enabling denial-of-service via slow or hung backends. Set a reasonable timeout (e.g., 30 seconds).

- **Loopback-Only Binding**: The gateway binds only to the loopback interface, which limits exposure to local processes. This is a positive security property and should be maintained.

- **Plaintext Internal Traffic**: Traffic between the gateway and backend apps is unencrypted. Since all traffic stays on the loopback interface, this is acceptable for single-host deployments but may be insufficient for distributed setups.
````

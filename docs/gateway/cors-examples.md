# CORS Configuration Examples

This guide shows how to configure Cross-Origin Resource Sharing (CORS) for Wasm Cloud Platform routes.

## What is CORS?

CORS is a browser security mechanism that controls which origins can access your API. When a browser makes a cross-origin request, it sends a **preflight** `OPTIONS` request to check if the actual request is allowed.

The platform's built-in API Gateway handles CORS preflight at the proxy layer — the upstream Wasm app never sees `OPTIONS` requests.

## Configuration Format

```toml
[app.gateway.cors]
allowed_origins = ["https://app.example.com"]
allowed_methods = ["GET", "POST", "PUT", "DELETE"]
allowed_headers = ["Authorization", "Content-Type"]
expose_headers = ["X-Request-Id"]
allow_credentials = true
max_age_secs = 3600
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `allowed_origins` | Yes | — | List of allowed origins, or `["*"]` for any |
| `allowed_methods` | No | `GET, POST, PUT, DELETE, PATCH, OPTIONS` | Allowed HTTP methods |
| `allowed_headers` | No | `Authorization, Content-Type, X-Request-Id` | Allowed request headers |
| `expose_headers` | No | `[]` | Headers exposed to JavaScript in the browser |
| `allow_credentials` | No | `false` | Allow cookies and Authorization header |
| `max_age_secs` | No | `86400` (24h) | How long browsers cache the preflight response |

## Examples

### Public API (no auth, open CORS)

For a public API that any website can call:

```toml
[app]
id = "public-api:v1"
wasm_bind_port = 8080

[app.gateway.cors]
allowed_origins = ["*"]
max_age_secs = 86400
```

**Caution**: `allowed_origins = ["*"]` cannot be combined with `allow_credentials = true`. The gateway rejects this combination.

### Single-Page Application (SPA)

For a React/Vue/Angular app hosted on a specific domain:

```toml
[app]
id = "api-users:v2"
wasm_bind_port = 8080

[app.gateway.auth]
policy = "authenticated"

[app.gateway.cors]
allowed_origins = ["https://app.example.com"]
allowed_methods = ["GET", "POST", "PUT", "DELETE"]
allowed_headers = ["Authorization", "Content-Type", "X-Request-Id"]
allow_credentials = true
max_age_secs = 3600
```

The browser will:
1. Send `OPTIONS /users` with `Origin: https://app.example.com`
2. The gateway responds with `200 OK` + CORS headers
3. The browser then sends the actual `GET /users` with the `Authorization` header

### Multiple Origins

For an API accessed by multiple domains:

```toml
[app.gateway.cors]
allowed_origins = [
    "https://app.example.com",
    "https://admin.example.com",
    "https://partner.example.com",
]
allow_credentials = true
max_age_secs = 3600
```

### Subdomain Wildcard

To allow all subdomains of a domain:

```toml
[app.gateway.cors]
allowed_origins = ["*.example.com"]
allow_credentials = true
max_age_secs = 3600
```

This matches:
- `https://app.example.com` ✓
- `https://admin.example.com` ✓
- `https://api-v2.example.com` ✓
- `https://example.com` ✗ (does not match `*.example.com`)

### Internal Service (no CORS)

For a backend service only called by other backend services (not browsers):

```toml
[app]
id = "payment-processor:v1"
wasm_bind_port = 8080

[app.gateway.auth]
policy = "authenticated"

# No [app.gateway.cors] section = no CORS handling
```

Without CORS config, cross-origin browser requests will be blocked by the browser (not by the gateway). Same-origin or backend-to-backend requests work normally.

### Mobile App Backend

For a mobile app (iOS/Android) that uses the API:

```toml
[app]
id = "mobile-api:v1"
wasm_bind_port = 8080

[app.gateway.auth]
policy = "authenticated"

[app.gateway.cors]
allowed_origins = ["*"]
max_age_secs = 86400
```

Mobile apps typically don't enforce CORS, but using `allowed_origins = ["*"]` ensures that any web views or hybrid apps (Cordova, Capacitor) can also access the API.

## Credentials and Origins

When `allow_credentials = true`, the `allowed_origins` **must** be a specific origin (not `"*"`). This is a browser security requirement.

**Valid**:

```toml
allowed_origins = ["https://app.example.com"]
allow_credentials = true
```

**Invalid** (gateway returns config validation error):

```toml
allowed_origins = ["*"]
allow_credentials = true
```

## CLI Commands

```bash
# Set CORS for a route
wasm-ctl gateway set-cors api-users:v2 \
  --origins "https://app.example.com,https://admin.example.com" \
  --credentials \
  --max-age 3600

# View current CORS config
wasm-ctl gateway show api-users:v2

# Remove CORS config (revert to no CORS)
wasm-ctl gateway reset api-users:v2
```

## Debugging CORS Issues

### Browser console shows "CORS policy blocked"

1. Check that the origin is in `allowed_origins`
2. Verify that `allow_credentials` matches your request (withCredentials/fetch credentials)
3. Check the preflight response in browser DevTools → Network → OPTIONS request

### Preflight returns 403

- The origin is not in `allowed_origins`
- The `Access-Control-Request-Method` is not in `allowed_methods`
- The `Access-Control-Request-Headers` contains headers not in `allowed_headers`

### Credentials not sent

- Ensure `allow_credentials = true` in the config
- Ensure `allowed_origins` is specific (not `"*"`)
- In JavaScript, set `credentials: 'include'` (fetch) or `withCredentials = true` (XHR)

## Response Headers

For allowed requests, the gateway adds these headers:

| Header | Value |
|--------|-------|
| `Access-Control-Allow-Origin` | Matched origin (or `*`) |
| `Access-Control-Allow-Methods` | Comma-separated methods |
| `Access-Control-Allow-Headers` | Comma-separated headers |
| `Access-Control-Allow-Credentials` | `true` (if configured) |
| `Access-Control-Expose-Headers` | Comma-separated exposed headers |
| `Access-Control-Max-Age` | Cache duration in seconds |

## Preflight Caching

Browsers cache preflight responses for `max_age_secs`. A value of `86400` (24 hours) is recommended for stable APIs. During development, set it lower (e.g., `60`) so config changes take effect quickly.

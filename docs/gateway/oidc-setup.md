# OIDC Setup Guide — Keycloak

This guide walks through configuring Keycloak as the OIDC identity provider for the Wasm Cloud Platform's built-in API Gateway.

## Prerequisites

- A running Keycloak instance (version 20+ recommended)
- Admin access to the Keycloak Admin Console
- The platform node configured with the Keycloak issuer URL

## Step 1: Create a Realm

1. Log in to the Keycloak Admin Console (`https://keycloak.example.com/admin`)
2. Click the realm dropdown (top-left) → **Create realm**
3. Enter realm name: `my-realm`
4. Click **Create**

## Step 2: Create a Client

1. In the realm, go to **Clients** → **Create client**
2. **General Settings**:
   - Client type: `OpenID Connect`
   - Client ID: `my-platform-api`
   - Click **Next**
3. **Capability config**:
   - Client authentication: **ON**
   - Authorization: **OFF** (not needed for basic JWT validation)
   - Authentication flow: **Standard flow** ✓, **Direct access grants** ✓
   - Click **Next**
4. **Login settings**:
   - Valid redirect URIs: `https://app.example.com/*`
   - Web origins: `https://app.example.com`
   - Click **Save**

## Step 3: Configure the Client

1. Go to the **Settings** tab of the new client
2. Set **Access settings**:
   - Root URL: `https://app.example.com`
   - Base URL: `/`
3. Go to the **Credentials** tab
4. Copy the **Client secret** — you'll need it for client authentication

## Step 4: Create Roles

1. Go to **Realm roles** → **Create role**
2. Create roles:
   - `admin`
   - `user`
   - `readonly`
3. Go to **Clients** → `my-platform-api` → **Roles** tab
4. Create client-specific roles if needed (e.g., `api-admin`, `api-user`)

## Step 5: Assign Roles to Users

1. Go to **Users** → select a user → **Role mapping** tab
2. Click **Assign role**
3. Filter by realm roles or client roles
4. Select roles and click **Assign**

## Step 6: Configure the Platform Node

Add the `[gateway.oidc]` section to your node's `config.toml`:

```toml
[gateway.oidc]
issuer_url = "https://keycloak.example.com/realms/my-realm"
audience = "my-platform-api"
jwks_refresh_secs = 3600
clock_skew_secs = 30
```

Or set via environment variables:

```bash
# Not directly supported; use TOML file or admin API
```

### Configuration Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `issuer_url` | — | Keycloak realm URL (required) |
| `audience` | — | Client ID, must match `aud` claim in JWT (required) |
| `jwks_refresh_secs` | 3600 | How often to refresh the signing key cache |
| `clock_skew_secs` | 30 | Tolerance for clock differences between nodes |

## Step 7: Verify JWKS Endpoint

The platform fetches signing keys from Keycloak's JWKS endpoint:

```bash
curl https://keycloak.example.com/realms/my-realm/protocol/openid-connect/certs
```

You should see a JSON response with a `keys` array containing RSA public keys.

## Step 8: Test Token Validation

Obtain a token from Keycloak:

```bash
# Using client credentials flow
curl -X POST https://keycloak.example.com/realms/my-realm/protocol/openid-connect/token \
  -H "Content-Type: application/x-www-form-urlencoded" \
  -d "grant_type=client_credentials" \
  -d "client_id=my-platform-api" \
  -d "client_secret=<CLIENT_SECRET>"
```

Decode the token to verify claims:

```bash
# Using jwt.io or command line
echo "<ACCESS_TOKEN>" | cut -d. -f2 | base64 -d | jq .
```

Expected claims:

```json
{
  "sub": "<user-id>",
  "iss": "https://keycloak.example.com/realms/my-realm",
  "aud": "my-platform-api",
  "realm_access": {
    "roles": ["admin", "user"]
  },
  "resource_access": {
    "my-platform-api": {
      "roles": ["api-admin"]
    }
  }
}
```

## Step 9: Deploy with Auth

```bash
wasm-ctl deploy \
  --app api-users \
  --version v2 \
  --wasm api-users.wasm \
  --gateway-auth roles \
  --gateway-roles admin,user \
  --gateway-oidc-client api-users
```

Or via deploy manifest:

```toml
[app.gateway.auth]
policy = "roles"
allowed_roles = ["admin", "user"]
client_id = "api-users"
```

## Troubleshooting

### "OIDC not configured" error

- Verify `[gateway.oidc]` is present in the node config
- Restart the node after adding OIDC config (not hot-reloadable)

### "unknown key ID" error

- Check that the JWT's `kid` header matches a key in the JWKS endpoint
- Verify `jwks_refresh_secs` is not too high (keys may have rotated)
- Force a JWKS refresh by restarting the node

### "JWT validation failed: ExpiredSignature"

- Check system clocks are synchronized across nodes (use NTP)
- Increase `clock_skew_secs` if clock drift is expected

### "wrong audience" error

- Verify `audience` in node config matches the Keycloak client ID
- Check the JWT's `aud` claim using jwt.io

## Other OIDC Providers

The platform works with any OIDC-compliant provider:

- **Auth0**: `issuer_url = "https://mytenant.auth0.com/"`
- **Okta**: `issuer_url = "https://mycompany.okta.com/oauth2/default"`
- **Dex**: `issuer_url = "https://dex.example.com"`
- **Azure AD**: `issuer_url = "https://login.microsoftonline.com/<tenant-id>/v2.0"`

The only requirement is that the provider supports the JWKS endpoint (`/.well-known/jwks.json` or equivalent) and issues RS256-signed JWTs.

## Security Best Practices

1. **Always use HTTPS** for the issuer URL
2. **Pin the CA certificate** in production (optional, configure in node TLS settings)
3. **Monitor JWKS refresh failures** — alert if the cache cannot be refreshed
4. **Rotate client secrets regularly** in Keycloak
5. **Use short-lived access tokens** (5–15 minutes) with refresh tokens
6. **Do not forward the raw JWT** to upstream apps unless explicitly needed

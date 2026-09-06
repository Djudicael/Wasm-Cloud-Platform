# Step 48 - Deploy Ingress and Remote Artifact Fetch

## Goal

Replace the current "operator uploads a local `.wasm` to one node" deployment
shape with a proper deploy-ingress flow:

1. CI publishes the Wasm artifact to an external artifact location.
2. CI sends a signed/authenticated deploy intent to the platform.
3. The platform fetches the artifact, verifies it, stores it internally, and
   publishes the normal deploy event.
4. Nodes deploy from the platform artifact plane, not directly from GitHub
   Packages or a CI runner.

This keeps GitHub-hosted CI usable without a self-hosted runner and avoids
making every node hold external registry credentials.

---

## Status Summary

Step 48 is **implemented at the core feature level** and **not fully closed at
the hardening/documentation tail**.

Status legend used below:

- `[x]` implemented and validated
- `[~]` implemented but intentionally narrower than the ideal end-state
- `[ ]` deferred / not yet completed

High-level assessment:

- remote deploy ingress: implemented
- remote HTTP artifact fetch: implemented
- OCI artifact fetch: implemented
- signature verification and verification records: implemented
- HA leader/follower and failover behavior: implemented
- multi-node fanout after failover: implemented
- GitHub-hosted CI deployment path: implemented
- remaining work: mostly hardening, policy/docs refinement, and optional supply
  chain evolution

---

## Current State

Today `wasm-ctl deploy` expects a local file:

- reads `--wasm <path>` from disk,
- computes the SHA-256 digest,
- uploads it to one node artifact endpoint,
- asks that node for per-node artifact manifests,
- publishes `DeployApp`.

That is workable for manual operation, but awkward for CI/CD:

- CI must reach one node admin/artifact endpoint,
- CI must know which node URL to use,
- private platform clusters need either public node admin exposure or a
  self-hosted runner,
- the artifact ingress point is a node, even though nodes are meant to follow a
  shared-nothing operational model.

---

## Target Model

Introduce a deploy-ingress component/API owned by the platform control plane.

CI sends:

- app identity,
- namespace,
- manifest/config,
- immutable artifact reference,
- expected digest,
- optional artifact credential reference,
- optional signature/provenance metadata.

The platform then:

- authenticates the deploy request,
- resolves any artifact-fetch credential by reference,
- downloads the artifact,
- verifies digest and policy,
- stores the artifact in the platform artifact store,
- issues internal artifact transfer metadata,
- publishes the existing `DeployApp` event.

Nodes continue to:

- fetch from the platform artifact plane,
- compile to serialized Wasmtime artifacts locally,
- store their own compiled artifact in local state,
- resolve runtime app secrets from platform secret storage at spawn time.

---

## Security Model

### Separation of Secrets

There are two different secret classes.

| Secret class | Used by | Purpose | Should appear in deploy event? |
|--------------|---------|---------|--------------------------------|
| Artifact fetch credential | Deploy ingress | Pull `.wasm` from GitHub Packages, GHCR, S3, etc. | No, only reference |
| Runtime app secret | Node runtime | Inject `DATABASE_URL`, API keys, JWT secrets | No, only reference |

Runtime app secrets must remain in the existing platform secret store. The
deploy manifest should contain only references:

```toml
[secrets]
DATABASE_URL = { ref = "prod-postgres-url" }
```

Artifact fetch credentials should also be referenced, not embedded:

```toml
[artifact]
url = "https://github.com/org/repo/releases/download/v1/app.wasm"
sha256 = "..."
credential_ref = "github-packages-reader"
```

### Transport Security

The deploy ingress endpoint must require:

- TLS,
- authenticated deploy caller,
- request body size limits,
- replay protection or short-lived credentials,
- audit logging.

The deploy payload normally does not need separate field-level encryption if it
contains only references and metadata. If sensitive bootstrap material ever must
be sent inline, it should be encrypted to a platform deploy-ingress public key
or delivered through a response-wrapping style mechanism.

### Artifact Integrity

Artifact verification is mandatory:

- SHA-256 digest is required for URL-based artifacts.
- Mutable tags are not enough.
- OCI references should prefer digest form:
  `oci://ghcr.io/org/app@sha256:<digest>`.
- Signature verification should be added as a policy layer, not as a substitute
  for digest verification.

---

## Proposed API

### Deploy Intent Request

Initial HTTP shape:

```json
{
  "app_id": "payments:v1",
  "namespace": "production",
  "manifest": {
    "app": {},
    "fuel": {},
    "policy": {},
    "gateway": {},
    "secrets": {}
  },
  "artifact": {
    "kind": "http",
    "url": "https://github.com/org/repo/releases/download/v1/payments.wasm",
    "sha256": "012345...",
    "credential_ref": "github-packages-reader"
  }
}
```

Future OCI shape:

```json
{
  "app_id": "payments:v1",
  "namespace": "production",
  "manifest": {},
  "artifact": {
    "kind": "oci",
    "ref": "oci://ghcr.io/org/payments-wasm@sha256:012345...",
    "credential_ref": "ghcr-prod-reader"
  }
}
```

### CLI Shape

Keep the current local upload path:

```bash
wasm-ctl deploy --manifest app.toml --wasm ./app.wasm
```

Add remote deploy intent:

```bash
wasm-ctl deploy \
  --manifest app.toml \
  --artifact-url https://github.com/org/repo/releases/download/v1/app.wasm \
  --sha256 <digest> \
  --artifact-credential github-packages-reader
```

Later:

```bash
wasm-ctl deploy \
  --manifest app.toml \
  --artifact oci://ghcr.io/org/app@sha256:<digest> \
  --artifact-credential ghcr-prod-reader
```

---

## Implementation Plan

### Phase 1 - Data Model

- [x] Add an `ArtifactSource` model in `common`.
- [x] Support at least:
  - `LocalUpload` for the current flow,
  - `HttpUrl` for deploy-ingress fetch,
  - reserved `OciRef` variant for later.
- [x] Add fields:
  - artifact kind,
  - URL/ref,
  - expected SHA-256,
  - optional credential reference,
  - optional signature metadata.
- [x] Add manifest support for `[artifact]`.
- [~] Keep `wasm_artifact` for compatibility during the transition.
  - Current deploy flow supports the remote artifact section and the existing
    local `--wasm` path.
  - Compatibility exists operationally; this was not implemented as a large
    parallel manifest-model migration.

### Phase 2 - Deploy Ingress API

- [x] Add a deploy-ingress HTTP endpoint.
- [~] Authenticate the caller using the existing admin/security model or a new
      deploy-token scope.
  - The implemented model uses the existing bearer-token permission split:
    read token for `GET`, write token for mutating deploy actions.
  - Endpoint auth behavior is now documented and covered by E2E tests.
  - This remains `[~]` because there is still no distinct deploy-only token type
    beyond the existing read/write bearer model.
- [x] Accept manifest + artifact source + expected digest.
- [x] Reject payloads that include raw runtime secret values.
- [x] Enforce request size limits.
- [x] Emit structured audit events:
  - caller identity,
  - app ID,
  - namespace,
  - artifact source kind,
  - digest,
  - result.
  - Note: auth exists and is configurable, but the production auth story could
    still be tightened and documented more explicitly.

### Phase 3 - Artifact Fetcher

- [x] Implement HTTP(S) artifact fetch first.
- [x] Resolve `credential_ref` through the platform secret store.
- [x] Support public URLs with no credential.
- [x] Support bearer-token style credentials for private GitHub Release/package
      URLs.
- [~] Stream artifact download with a maximum size limit.
  - Maximum size enforcement exists and fails closed.
  - The implementation is hardened enough for the current platform, but not yet
    a separately documented "streaming pipeline" design.
- [x] Compute SHA-256 while streaming.
- [x] Reject if computed digest differs from expected digest.
- [x] Store verified artifact in the existing artifact store.
- [x] Reuse current artifact authorization/manifest fanout path after storage.

### Phase 4 - Deploy Event Integration

- [x] After successful artifact ingest, publish the normal `DeployApp` event.
- [x] Preserve existing fields needed by nodes:
  - app config,
  - artifact URL in the platform artifact plane,
  - expected hash,
  - artifact transfer manifests,
  - size.
- [x] Do not make nodes fetch from GitHub Packages in the first version.
- [x] Keep current local-upload deploy path working.

### Phase 5 - CLI Support

- [x] Add `wasm-ctl deploy --artifact-url`.
- [x] Require `--sha256` when `--artifact-url` is used.
- [x] Add `--artifact-credential <ref>` for private sources.
- [x] Route remote deploys to the deploy-ingress API.
- [x] Keep `--wasm` behavior unchanged.
- [x] Add clear errors when users pass both `--wasm` and `--artifact-url`.

### Phase 6 - CI/CD Documentation

- [x] Add a GitHub Actions example using GitHub-hosted runners.
- [x] Show build + publish artifact + deploy intent.
- [x] Document required secrets:
  - platform deploy token,
  - optional artifact credential reference already stored in the platform.
- [x] Make clear that CI does not need a node URL if deploy ingress is exposed.
- [x] Make clear that runtime app secrets are managed separately.

### Phase 7 - OCI/GHCR Support

- [x] Add `oci://` artifact refs.
- [x] Pull Wasm artifact blobs from GHCR or another OCI registry.
- [x] Require digest-pinned references for production mode.
  - Digest refs are supported directly.
  - Tag refs remain usable by default.
  - Hardened mode can now require digest-pinned refs with
    `WASM_DEPLOY_INGRESS_REQUIRE_OCI_DIGEST_REFS=true`.
- [x] Support registry credentials by `credential_ref`.
- [x] Add optional signature verification policy.
- [x] Add tests for mutable-tag rejection in hardened mode.

### Phase 8 - Signature and Provenance Policy

- [x] Add optional artifact signature metadata.
- [x] Support signature verification before artifact storage.
- [x] Add policy knobs:
  - digest required,
  - signature required,
  - allowed issuers,
  - allowed identities,
  - allowed repositories,
  - allowed namespaces.
- [x] Store verification result with artifact metadata.
- [x] Expose verification status through admin API.
  - Current implementation uses a platform-native Ed25519 signed-claims model.
  - Full Sigstore/Cosign interoperability is deferred.

---

## Minimal Viable Implementation

The first useful implementation should include:

- `--artifact-url`,
- required `--sha256`,
- optional `--artifact-credential`,
- platform-side HTTP(S) fetch,
- digest verification,
- internal artifact storage,
- existing `DeployApp` publish path,
- documentation for GitHub Actions.

This is enough to remove the need for CI to upload a local `.wasm` to a chosen
node while avoiding the larger OCI/signature work.

Current state:

- This minimum implementation is complete.
- The implementation has also moved beyond the minimum with:
  - OCI support,
  - signature policy,
  - HA deploy-ingress behavior,
  - failover coverage,
  - multi-node fanout validation.

---

## Non-Goals for the First Pass

- No direct node pulls from GitHub Packages.
- No raw registry tokens in deploy events.
- No raw app runtime secrets in deploy events.
- No requirement to replace the existing local `--wasm` deploy path.
- No OCI support in the minimum implementation.
- No mandatory Sigstore/Cosign integration in the minimum implementation.

---

## Acceptance Criteria

- [x] GitHub-hosted CI can trigger a deploy without knowing a node artifact URL.
- [x] CI sends only deploy intent, artifact reference, digest, and metadata.
- [x] Platform fetches the artifact itself.
- [x] Artifact digest mismatch fails closed.
- [x] Private artifact fetch uses `credential_ref`, not inline token material.
- [x] Runtime app secrets remain referenced by name and resolved by nodes at
      spawn time.
- [x] Existing local `wasm-ctl deploy --wasm` still works.
- [x] Nodes continue to deploy from platform-local artifact storage.
- [x] Tests cover public URL fetch, private URL fetch, digest mismatch, missing
      credential, deploy event emission, and primary auth/policy rejection
      paths.
  - Critical positive and negative cases are covered at the deploy-ingress
    HTTP boundary.
  - Remaining work here is edge-case expansion, not missing primary coverage.

---

## Test Plan

### Unit Tests

- [x] Manifest parses `[artifact]`.
- [x] CLI rejects invalid argument combinations.
- [x] Artifact source validation rejects mutable/undigested refs where required.
  - URL-based sources require digest.
  - OCI hardened-mode rejection is enforced when
    `WASM_DEPLOY_INGRESS_REQUIRE_OCI_DIGEST_REFS=true`.
- [x] Secret validation rejects inline runtime secret values.

### Integration Tests

- [x] Deploy ingress fetches a local test HTTP artifact and stores it.
- [x] Deploy ingress rejects digest mismatch.
- [x] Deploy ingress uses a stored credential for private fetch.
- [x] Deploy ingress publishes the expected `DeployApp`.
- [x] Node deploys from the resulting platform-local artifact URL.

### E2E Tests

- [x] Build Wasm app.
- [x] Serve it from a mock remote artifact source.
- [x] Submit deploy intent with URL + SHA-256.
- [x] Confirm route serves the app through the proxy.
- [~] Confirm no raw runtime secret value appears in logs or deploy event.
  - Runtime secret references are enforced structurally.
  - A dedicated end-to-end log/event leakage assertion for every path is still
    deferred.

Additional E2E coverage now exists beyond the original test plan:

- [x] OCI tag and digest deploy path
- [x] signed artifact acceptance path
- [x] signature-policy rejection path
- [x] deploy-ingress accepted/rejected audit assertions
- [x] HA follower write rejection with leader hint
- [x] HA promotion after leader exit
- [x] replicated credential use after failover
- [x] multi-node artifact fanout after failover
- [x] deploy-ingress read/write auth boundary rejection and read-token
      separation

---

## Operational Notes

- In production, deploy ingress should be the public/control-plane deployment
  endpoint, not the node artifact server.
- Nodes should not need GitHub/GHCR credentials for the first implementation.
- Artifact fetch credentials should be rotated like any other platform secret.
- Digest verification should be logged and auditable.
- Deploy intent should be idempotent for the same app ID + artifact digest.

---

## Deferred Follow-Up Work

These items are intentionally **not** blocking Step 48 from being treated as
implemented, but they are still useful hardening follow-ups:

- [x] document the deploy-ingress auth model more explicitly for production
      operators
  - Endpoint permissions, status-code behavior, CI token usage, and
    rotation/topology guidance are documented.
  - A dedicated operator runbook now exists for deploy-ingress operations.
- [~] add broader negative-path coverage for auth/policy/size failures
  - Primary auth and policy rejection paths are covered, including malformed
    auth and oversized-body rejection.
  - Additional edge-case coverage is still useful.
- [~] evaluate Sigstore/Cosign interoperability on top of the current native
      Ed25519 verification path
  - Public-key verification of Cosign-style payloads is supported through the
    `cosign-ed25519` signature mode.
  - Sigstore bundle verification is supported through the `sigstore-bundle`
    signature mode with issuer/identity policy binding.
  - Full custom trust-root / enterprise Sigstore deployment support is still
    deferred.
- [x] add stronger API contract documentation for deploy-intent payloads and
      responses

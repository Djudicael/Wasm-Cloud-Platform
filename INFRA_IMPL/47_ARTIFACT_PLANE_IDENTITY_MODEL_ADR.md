# Step 47 — ADR: Artifact-Plane Identity Model

## Status

**Accepted — phase 1 implemented; later provenance/transport hardening remains**

---

## Context

After the hardening work captured in Steps 42–46, the artifact plane is no longer an obvious release blocker:

- artifact serving is loopback-by-default,
- remote access is no longer anonymous,
- advertised artifact endpoints are explicit,
- and peer transfer is now authenticated enough to be secure-by-default.

The main remaining question is no longer “is the artifact plane exposed?” but rather:

**what should the long-term identity model for artifact transfer be?**

This ADR compares three viable directions:

1. `bearer + scoped expiring tokens`
2. `signed short-lived transfer manifests`
3. `mTLS between nodes`

---

## Decision Drivers

- Keep the platform secure-by-default
- Preserve the shared-nothing / low-external-dependency design
- Improve artifact-plane identity beyond simple bearer possession where practical
- Keep bootstrap and node-join flows operable in real clusters
- Prefer least-privilege, short-lived authorization
- Leave room for stronger provenance and attestation later
- Avoid introducing operational PKI complexity earlier than necessary

---

## Options Considered

### 1. Bearer + scoped expiring tokens

**Summary**

A node presents a short-lived bearer token authorizing artifact upload/download with limited scope.

**Pros**

- Simplest model operationally
- Already close to the current implementation direction
- No CA or certificate lifecycle required
- Easy to roll out incrementally
- Works well for same-host and small-cluster deployments

**Cons**

- Identity is still fundamentally “whoever possesses the token”
- Token theft or replay remains possible until expiry
- Authorization is usually coarser than the exact transfer intent
- Weaker audit semantics than a signed per-transfer authorization object
- Does not materially strengthen artifact provenance by itself

**Assessment**

Good near-term default and compatibility path, but not the strongest long-term artifact identity model.

---

### 2. Signed short-lived transfer manifests

**Summary**

Each transfer uses a signed manifest that authorizes a specific action for a very short time window, for example:

- artifact digest
- allowed method (`GET` / `PUT`)
- source and/or destination node identity
- path or artifact locator
- expiry time
- transfer ID / nonce

The receiving node verifies the signature and manifest contents before serving the artifact.

**Pros**

- Stronger than bearer tokens because authorization is bound to a specific transfer
- Better least-privilege properties
- Good fit for short-lived bootstrap and replication flows
- Easier to align with future provenance work and signed artifact metadata
- Does not require mandatory cluster-wide mTLS/CA infrastructure
- Preserves a shared-nothing verification model using configured public keys

**Cons**

- More design and implementation complexity than tokens
- Requires canonical manifest structure and signature verification rules
- Replay protection needs explicit nonce / transfer-ID handling
- Still relies on configured signing trust roots
- Transport encryption/authentication is still separate unless layered with TLS/mTLS

**Assessment**

Best next architectural hardening step if the goal is to improve artifact-plane identity without taking on full PKI complexity.

---

### 3. mTLS between nodes

**Summary**

Every node presents a client certificate and verifies peer certificates for artifact-plane connections.

**Pros**

- Strong transport-level peer identity
- Eliminates bearer-token style possession semantics for node authentication
- Standard and well-understood security model
- Good foundation if the platform eventually wants a stronger node-to-node trust fabric everywhere

**Cons**

- Highest operational complexity
- Requires CA issuance, rotation, revocation, and bootstrap story
- Harder developer and small-cluster ergonomics
- More invasive rollout across cluster lifecycle flows
- Still benefits from request-level authorization if you want transfer-specific restrictions

**Assessment**

Strongest pure node-authentication option, but currently too heavy as the immediate next hardening step unless the platform is ready to standardize broader node PKI.

---

## Decision

Adopt **signed short-lived transfer manifests** as the **target artifact-plane identity model**.

Retain **bearer + scoped expiring tokens** as the **near-term compatibility and transitional mechanism**.

Defer **mandatory mTLS between nodes** until the platform has a broader, deliberate node PKI story beyond the artifact plane alone.

---

## Rationale

This choice is the best balance of security improvement, architectural fit, and operational cost.

Compared with bearer tokens, signed transfer manifests provide materially stronger authorization because they can bind access to:

- one artifact,
- one direction of transfer,
- one intended peer or audience,
- and one short expiration window.

Compared with mTLS, they deliver most of the next-value hardening without forcing the platform to solve full certificate issuance, rotation, revocation, and cluster bootstrap PKI right now.

This also aligns well with the remaining future-strengthening themes already identified in the audit:

- stronger artifact identity than token-based auth,
- stronger provenance models,
- and continued tightening of distributed trust boundaries.

---

## Consequences

### Positive

- Better artifact-plane identity than plain bearer possession
- Better fit for per-transfer authorization and audit logging
- Clean bridge toward provenance-aware artifact distribution
- No requirement for immediate full-cluster mTLS rollout

### Costs

- New manifest schema and signing rules must be designed carefully
- Replay handling and expiry validation must be explicit
- Key rotation and trust-root configuration still need operational guidance
- There will be a transition period where both manifests and bearer tokens may coexist

---

## Implementation Direction

### Phase 1 — Design and introduce manifests

Define a signed transfer manifest containing at least:

- artifact digest
- artifact path / locator
- allowed HTTP method
- issuer identity
- audience node ID or peer identity
- issued-at time
- expiry time
- transfer ID / nonce

**Implementation status**

- [x] Canonical signed transfer-manifest types now exist in `crates/common/src/artifact_transfer.rs`.
- [x] The artifact server now accepts signed manifest authorization in addition to the transitional bearer-token path.
- [x] Successful artifact uploads now return a short-lived signed GET manifest, primarily used by the deploy tool to identify the upload source node and request audience-bound follow-on authorizations.
- [x] Bootstrap snapshots now carry signed per-artifact fetch authorizations so fresh nodes can pull artifacts via signed GET instead of waiting for remote PUT push.
- [x] `NodeJoined` no longer provisions or advertises a special bootstrap artifact bearer token; artifact-server bootstrap setup now relies on signed fetch manifests instead.
- [x] `DeployApp` remote artifact fetch now relies on signed transfer manifests only; the bearer-token field has been removed.
- [x] `wasm-ctl deploy` now requests audience-bound per-node GET manifests from the artifact server and publishes them in `DeployApp`.
- [x] Nodes now fail closed for remote deploy fetch when their own audience-bound manifest is missing.
- [x] Cluster node discovery for deploy fan-out now uses an authoritative cluster-node registry exposed by `/admin/cluster/nodes`, with live two-node WSL validation covering deterministic audience-bound manifest fan-out.

### Phase 2 — Enforce manifest verification on remote artifact flows

- Require a valid manifest for remote non-loopback artifact access
- Reject expired, mismatched, or replayed manifests
- Log signer identity and transfer ID for auditability

### Phase 3 — Keep tokens only as a compatibility path

- Restrict bearer tokens to transitional or explicitly configured deployments
- Prefer manifests for cluster bootstrap, peer fetch, and replication flows

### Phase 4 — Revisit transport identity separately

If the platform later standardizes broader node PKI, mTLS can be layered underneath manifests rather than replacing them. In that model:

- **mTLS** proves peer node identity at the transport layer
- **signed manifests** authorize the specific artifact transfer

That combination is likely the strongest long-term design, but it should be adopted only when the platform is ready for the operational cost.

---

## Final Recommendation

For the **next best architectural hardening step**, design the artifact plane around **signed short-lived transfer manifests**.

Do **not** rush to mandatory mTLS yet unless you are ready to introduce a broader certificate-management model across the platform.

Keep the current token-based path as a practical bridge, but treat it as the interim model rather than the final one.

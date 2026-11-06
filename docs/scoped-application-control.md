# Scoped application control

The optional node-local `/control/v1` API supplies application-scoped service
credentials independently of the legacy global administration API. It contains
generic execution primitives; agent tools, provider selection and approval
workflows belong to the harness repository.

The API exposes scoped inventory, safe configuration and runtime observations,
reviewed application changes, deployment history and explicit node admission
controls. The harness provides the versioned `wcp-control-v1` and
`wcp-control-v2` adapters; `/control/v1` remains the platform protocol prefix.
External telemetry queries and their correlation belong to separately pinned
harness providers. Native compilation and unit tests are not deployed-node
acceptance. The expanded P2 surface passed deployed Source acceptance on
2026-11-06 for the documented local profiles; see
[verification evidence and scope](#expanded-p2-verification-status).

## Enable a separate listener

The API is disabled unless both `--control-grants-file FILE` and
`--control-port PORT` are supplied. `--control-bind-address` defaults to
`127.0.0.1`. The listener exposes only `/control/v1`; it does not expose global
admin, artifact or deploy-ingress endpoints.

The grants JSON must be a regular file owned by the node user, mode `0600`,
at most 64 KiB. Symlinks and invalid scopes are rejected. Its shape is:

```json
{
  "grants": [
    {
      "principal": "application-observer",
      "token": "<64 hexadecimal characters generated from 32 random bytes>",
      "applications": ["example/service:v1"],
      "permissions": ["observe"]
    }
  ]
}
```

Use different principals/tokens for observation and interventions. Permissions
are independent: `observe`, `restart`, `configure`, `deploy`, `node_observe`
and `node_manage`. Application IDs are exact, qualified
`namespace/name:version` strings; wildcards are unsupported. There are at most
64 principals and 32 applications per principal. Node grants use an optional
`nodes` array of at most 32 exact node IDs; node permissions do not imply
application permissions. A grant must name applications or nodes. These service credentials are
opaque bearer tokens, not OIDC tokens. The harness workflow's OIDC identities
remain a separate authorization boundary.

TLS uses the configured admin TLS material, including its documented proxy
certificate fallback. Plain HTTP requires `--control-allow-http`, a loopback
or private bind IP and a non-production node environment. The exception is for
explicit local development. Existing admin authentication and bind addresses
remain unchanged. Grant-file changes take effect after a node restart.

## Routes and permissions

| Method and path | Permission | Behavior |
| --- | --- | --- |
| `GET /control/v1/apps` | `observe` | Only deployed applications in the credential's exact scope |
| `GET /control/v1/apps/{app}` | `observe` | Safe snapshot, node identity, artifact hash and execution fence |
| `POST /control/v1/apps/{app}/restart` | `restart` | Conditional eviction of the reviewed instance set |
| `GET /control/v1/apps/{app}/operations/{operation}` | Saved operation permission | Read this principal/application's durable operation receipt |
| `GET /control/v1/apps/{app}/details` | `observe` | Runtime, probes, effective policy, routes, trigger statistics and binding audit |
| `GET /control/v1/apps/{app}/history?limit=16&beforeSequence=...` | `observe` | Actual deployment revisions; max 32 rows with continuation metadata |
| `GET /control/v1/apps/{app}/diff?from=...&to=...` | `observe` | Safe projections and change flags for actual revisions/authorized versions |
| `POST /control/v1/apps/{app}/plans` | `observe` or matching effect permission | Store a proposal without executing it |
| `GET /control/v1/apps/{app}/plans/{digest}` | `observe` or matching effect permission | Inspect the immutable proposal and its exact input hash |
| `POST /control/v1/apps/{app}/apply` | `configure` or `deploy` for the exact change | Apply the exact stored proposal after fence verification |
| `GET /control/v1/apps/{app}/flows?sinceMs=...&limit=32` | `observe` | Real attributed kernel TCP observations; max 128 requested rows |
| `GET /control/v1/apps/{app}/timeline?startMs=...&endMs=...` | `observe` | Observed deployments, control receipts and execution errors |
| `GET /control/v1/billing?namespace=...&startMs=...&endMs=...` | `observe` | Actual persisted usage for allowed applications in the namespace |
| `GET /control/v1/nodes` | `node_observe` | Allowed local/registered node metadata, with source node identity |
| `GET /control/v1/nodes/{node}` | `node_observe` | Local capacity, dependency health, admission flags and node fence |
| `GET /control/v1/nodes/{node}/storage` | `node_observe` | Actual database size, free filesystem bytes, artifacts and last GC observation |
| `GET /control/v1/release` | `node_observe` | Allowed versions and actual local running ELF SHA-256 |
| `POST /control/v1/nodes/{node}/changes` | `node_manage` | Conditional, reversible cordon/drain of this exact local node |
| `GET /control/v1/nodes/{node}/operations/{operation}` | `node_manage` | Same-principal durable node intervention receipt |

Percent-encode the application's slash in a path segment, for example
`example%2Fservice:v1`. An observer can persist a proposal but cannot execute
it. An intervention principal can inspect its permitted proposal and receipt;
this does not grant the general configuration-inspection permission. Neither
can access another application outside its exact grant.

Snapshot projection returns environment **names**, secret-binding **names**,
resource/placement/policy data and safe gateway settings. It does not return
environment values, resolved secrets, API-key records, transformation header
values or proxy upstream
addresses. Responses are limited to 12,000 serialized bytes; oversized results
fail explicitly rather than silently truncating a fence.

## Conditional restart and uncertain outcomes

Use the snapshot's exact `fence` as `expected`:

```json
{
  "operationId": "caller-owned-unique-operation",
  "expected": {
    "revision": "<snapshot content SHA-256>",
    "instances": ["<reviewed instance UUID>"]
  }
}
```

The revision hashes current configuration, gateway, artifact, routes, paused
triggers, circuit state and desired-instance metadata; it is
a content digest, not a monotonically increasing version. Instances are a set.
A root API restart accepts one to 128 instance IDs and a bounded operation key.
The historical harness `wcp-control-v1` contract retains its one-to-four budget;
`wcp-control-v2` supports the expanded fence without changing old contracts.

The node serializes with control-plane lifecycle updates, compares the fence,
then persists a dispatch boundary before touching execution. The supervisor's
spawn gate and pool lock compare and detach the exact instance set atomically.
If an in-flight spawn changed the set, no instance is evicted. After eviction,
normal traffic can lazily start replacement instances. A successful receipt
means the reviewed instances stopped; it does not attest application health.
Validate recovery with an actual request and compare replacement IDs.

Operation identity binds principal, application and operation key. Reusing the
key with a different request returns a conflict. A committed successful receipt
is returned on identical replay without evicting replacements. A crash or lost
receipt after dispatch stays unknown and never authorizes a second eviction.
Receipt lookup does not dispatch anything. The journal survives ordinary node
restart in redb; this is node-local idempotency, not cross-node consensus or
permission to fail over a mutation to another node.

The separate legacy `GET /admin/apps?namespace=...` inventory retains its
existing administration authentication and returns an additional optional
`artifactSha256` field per application, read from the persisted artifact hash.
It is null when no hash is recorded; a storage read failure returns an error
instead of attesting unknown bytes. This metadata can compare deployed WASI
artifacts with controller manifests without exposing environment or secrets.

Request bodies are limited to 8 KiB, except proposal creation (16 KiB). A busy mutation lane returns 429 instead
of creating an unbounded mutation queue. Failed authentication, application
scope, changed preconditions and uncertain outcomes have distinct HTTP statuses
and stable generic error codes; credentials and request bodies are not logged.

## Local integration evidence

The harness controller's opt-in `--with-platform-control` profile exercised this
surface through real WASI connectors with the real OIDC Hub on the native node.
Acceptance `a9b8612c-302f-4ac7-8508-fe46b54511b1` (2026-11-05) passed twelve
cases: seven reads, approved restart, stale-state refusal, committed restart
with lost acknowledgement recovered by receipt GET, and two scope refusals.
A normal stop/start preserved actual coordinator history and both provider
receipts; read-only revalidation reported zero new activities. The unchanged
Hub was redeployed from its existing artifacts, without rebuilding it.

This qualifies conditional eviction and the bounded observation profile only.
It does not demonstrate a complete incident diagnosis, production deployment,
telemetry search, distributed idempotency or the remaining catalogue controls.
The harness owns its private acceptance proof, public redacted summary and CLI
workflow; see its `docs/platform-control.md` and full P2.2 inventory.
The final rebuilt WASI adapter also passed the twelve-case gate as `20a40da0-cce6-40d1-9f68-621823cf2a95`, including the final baseline and owned-resource cleanup. This does not extend the acceptance scope described above.

## Exact proposals and application changes

Proposal creation accepts `{ "expected": {"revision":"...","instances":[]},
"change": {"kind":"..."} }`. Unknown nested fields that would otherwise be
ignored by legacy configuration deserialization are rejected at this boundary.
The proposal stores the exact application, source node, creator, reviewed
fence, typed change and five-minute expiry. Its response includes `planDigest`,
`changeSha256`, `expected`, a safe `change` projection, `expiresAtMs` and
`executionAuthorized: false`. `changeSha256` hashes the original exact input
change; the plan digest additionally binds its normalized typed form. Public
projections can omit secret-bearing fields without changing the approved hash.
There are at most 64 live proposals per application; expired proposals can be
removed during later proposal creation.

Apply accepts `{ "operationId":"...", "planDigest":"..." }`. It checks the
same local node, permission, expiry and exact current application fence before
persisting the dispatch journal. The successful receipt binds application,
node, operation, plan digest, exact change hash and `resultingFence`. Identical
committed replay consults the journal before proposal expiry/garbage collection.
An uncertain dispatch remains uncertain; receipt GET never executes a change.
Existing retained v1 receipts keep their original schema and key derivation.

| Change `kind` | Additional fields | Effect permission |
| --- | --- | --- |
| `configure` | `key`, integer `value` | `configure` |
| `scale` | integer `replicas` | `configure` |
| `deploy` | `artifactSha256`, optional typed `config` | `deploy` |
| `rollback` | `revision` (actual archived digest or explicit authorized version) | `deploy` |
| `route` | `host`, `pathPrefix`, `stripPrefix` | `configure` |
| `gateway` | typed `config` | `configure` |
| `gateway_reset` | none | `configure` |
| `trigger` | declared trigger `id`, boolean `paused` | `configure` |

Configuration overrides are restricted to `fuel_quota` (1..10 billion),
`memory_pages` (1..8192), `max_instances` (1..100) and
`idle_timeout_secs` (1..86400), subject to node admission and memory budgets.
Scale accepts 0..100. Zero persists an admission pause and stops the reviewed
instances while retaining a valid positive configuration ceiling. Positive
scale persists the desired count, replaces the reviewed pool and reconciles
actual instances; cold starts also enforce the real pool ceiling. HTTP waiter
cancellation does not drop detached shutdown handles or release their application
spawn/capacity gate: shutdown continues in an owned task, with concurrent bounded
grace waits. Guests whose exit times out
remain fenced and tracked until reaped. A failed or uncertain shutdown does
not produce a successful receipt.

Deploy references raw WASM already present in the operator-managed artifact
cache, verifies its SHA-256 and compiles it locally at apply time. It accepts no
arbitrary URL or filesystem path. A new exact granted application requires its
full typed configuration and the canonical absent fence from `/details`.
The legacy `GET /apps/{app}` still returns 404 for an absent application;
`/details` returns `present:false` and the absent fence. Neither treats an
undeployed application with still-tracked guests as cleanly absent.
Configuration ID/namespace must match the target. Existing secret bindings and
local dependencies can be preserved. New secret bindings must be explicitly
listed in the grant's `secretKeys`; new dependencies must be same-namespace,
valid for local placement and independently present in its exact
`applications` grant. Secret binding names are not secret values.

Route changes additionally require the exact host in the grant's `routeHosts`.
Wildcards/default hosts and `.internal` hosts are unsupported; an existing
foreign application's host cannot be stolen. This is a scoped edge-routing
primitive; the same-node `.internal` mesh boundary remains unchanged.
Gateway reset resets only the reviewed circuit state and preserves authentication
configuration. Gateway circuit settings and local route rate limits take effect
in the actual proxy. A 0/0 local route limit produces real 429 responses.
Scoped changes requesting `distributed:true` rate limits are rejected with 422:
the existing distributed limiter has an operator-managed startup/NATS KV
lifecycle, which this API does not silently reconfigure.

## Node controls and protected diagnostics

Node intervention accepts:

```json
{
  "operationId": "caller-owned-operation",
  "expectedRevision": "<reviewed node revision SHA-256>",
  "change": {"kind": "drain", "enabled": true}
}
```

`kind` is `cordon` or `drain`; `enabled:false` explicitly reverses that flag.
The revision binds operator-controlled admission/deployments and exact execution
sets, not changing latency, uptime or metric counters. A node write gate fences
concurrent starts during comparison and intervention. Cordon prevents new
execution. Drain also independently rejects edge admission and retires reviewed
instances; future schedule starts are gated. Resume never clears critical/OOM
backpressure, and memory recovery never clears an operator drain. Flags survive
ordinary node restart. Node mutations are local only: a mismatched node ID is
refused rather than forwarded. Their receipt includes `expectedRevision`, the
resulting state and operation identity. Test them on a separately owned native
node, never on the shared OIDC/coordinator node.

`/details` reports real live instance states and probe observations. Instance
metadata is limited to 16 entries with complete counts and an explicit
`instancesTruncated` flag. Trigger statistics are persisted from actual scheduler
outcomes and include counts and last fire/completion times. Pausing cannot undo
an invocation already in flight; allow it to settle before comparing statistics.
Binding audit reports names, presence and missing/unbound bindings without
returning values. Configuration/plan/history projections never return environment
values or transformation header credentials.

Deployment revisions are committed atomically with the actual config/artifact
update in redb. History has no synthetic backfill for deployments before this
feature. `/history` returns summary rows and `nextBeforeSequence` when truncated;
`/details` includes only the most recent eight summaries. Diff compares actual
archived configurations/artifacts, exposing safe before/after projections plus
field/artifact/environment-value-change flags. Rollback loads the real archived
configuration and cached raw artifact; unavailable sources fail explicitly.

Usage windows are at most 24 hours. Billing reads the actual persisted completed
execution records for allowed apps, with a bounded source scan of 4096 records;
a larger source fails instead of returning incomplete totals. Optional grant-file
pricing is operator configuration, for example:

```json
{
  "pricing": {
    "currency": "EUR",
    "fuelMicrosPerMillion": 1,
    "memoryMicrosPerGibSecond": 2
  },
  "grants": []
}
```

Replace the empty illustrative `grants` array with real private grants before
use. Pricing produces a checked integer microcurrency estimate and tariff hash;
no pricing means `pricingConfigured:false` and no invented charge. Reserved RAM
byte-time is allocation accounting, not measured process RSS. Storage usage and
GC fields come from actual filesystem/store observations; an absent GC record
is null. Release provenance verifies the local running ELF hash. Remote registry
metadata is explicitly not verified binary provenance.

Network flows require active, non-degraded kernel monitoring; unavailable eBPF
returns 503. Rows come from a bounded 1024-record parsed kernel-event ring,
filtered by the application's registered TID identity, and preserve actual
addresses/ports. They attest an observed established TCP connection. A denied
pre-connect attempt has no TCP-state event; the API does not invent a denial
record or inferred policy verdict. Retention/truncation is explicit.

The raw control timeline combines actual deployment history, control journal
receipts and persisted execution errors. Windows are bounded to 24 hours,
source/event truncation is explicit and `requiresExternalAlertCorrelation:true`
remains set. Full alert/log/metric/trace correlation is the harness's separately
pinned provider composition; this platform endpoint does not fabricate external
observations. It also does not claim cumulative process restart counts or
complete lifetime error history from the live runtime snapshot.

For private native audit collection, `--audit-log-file PATH` redirects the
platform's default audit destination to an owned regular 0600 file opened with
no symlink following. The descriptor stays open; the global `/var/log` default
is preserved when the option is absent. Successful durable control operations
emit actual audit events. The collection envelope contains correlation fields;
protected local audit details are not copied into general telemetry logs.

## Expanded P2 verification status

The expanded controls compile in the native workspace and the harness v2
adapter checks explicitly for `wasm32-wasip2`. Focused tests cover actual history
transactions/pagination, reviewed circuit reset, local rate-limit recovery,
independent OOM/operator fences, cancellation-safe shutdown ownership, zero
admission, private audit-file protection, exact app-scoped flow projection and
retained v1 journal compatibility. The adapter's HTTP boundary tests cover
approved exact fences/change hashes, read-only lost-acknowledgement recovery
without a second mutation, and refusal to apply a gateway plan through the
deployment capability. Canonical BPF objects were rebuilt with the pinned eBPF
toolchain; native feature tests validate event/ABI handling.

Expanded Source acceptance through real deployed WASI connectors and the real
OIDC Hub passed on Linux/WSL on 2026-11-06. The accepted full P2 aggregate is
`f871e4da-3a6e-4052-84e9-db4c649f442d`; it includes the three platform shards,
observability and the real 429 incident/approved recovery. Its overall thirteen
groups cover 87 operations across 78 catalogue rows, with 146 scenarios
(108 positive, 38 expected refusals), 184 activities and 80 approvals. See the
[full acceptance report](../../wasmcloud_platform_harness/docs/evidence/p2-full-acceptance-2026-11-06.md),
[redacted evidence](../../wasmcloud_platform_harness/docs/evidence/p2-full-acceptance-2026-11-06.json)
and [platform operation inventory](../../wasmcloud_platform_harness/docs/p2-platform-observability-plan.md)
for the v2/provider profiles, exact references and limits.

Read-only aggregate verification passed before and after a canonical restart
with durable journals unchanged within each verification window. The normal
startup health workflow between those windows is accounted for separately in
the report. Fresh v1 compatibility also passed as
`9f3530f5-70db-4e7f-bfbc-877907d97dbb`; the earlier v1 references above remain
historical evidence of the smaller foundation profile.

Final [owned cleanup evidence](../../wasmcloud_platform_harness/docs/evidence/p2-final-cleanup-2026-11-06.json)
records Main stopped, all 25 recorded probes' temporary resources absent and
three persistent volumes retained. This is acceptance of the documented local
native-node/WASI/provider profiles, not a claim about production capacity,
external production accounts or remote hardware portability.

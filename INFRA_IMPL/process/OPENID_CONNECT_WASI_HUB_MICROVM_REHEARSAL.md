# OpenID-Connect-WASI-Hub microVM rehearsal

## Result

The application is running locally and is intentionally being retained for interactive testing.

- Application: `http://localhost:8088`
- Realm login: `http://localhost:8088/realms/master/login`
- HAProxy statistics: `http://localhost:8404/stats`
- Admin email: `admin@example.com`
- Admin password: `Admin123`
- State file: `.oidc-vm-testbed-state.json`

These credentials and the platform bearer token used below are local-test values. Never reuse them in production.

For the reusable gates that should be applied to any application, use the
[application deployment readiness checklist](./APPLICATION_DEPLOYMENT_READINESS_CHECKLIST.md).

## Validated topology

| Role | Address | Notes |
|---|---|---|
| HAProxy front door | `127.0.0.1:8088` | Browser entry point; application-aware health pools |
| Platform node 1 | `172.20.0.18` | Admin `9090`, artifact `9091`, embedded proxy `8080` |
| Platform node 2 | `172.20.0.19` | Admin `9090`, artifact `9091`, embedded proxy `8080` |
| Platform node 3 | `172.20.0.21` | Admin `9090`, artifact `9091`, embedded proxy `8080` |
| NATS microVM | `172.20.0.10:4222` | Separate from the requested platform-node count |
| PostgreSQL microVM | `172.20.0.20:5432` | PostgreSQL 17 with `pgcrypto` |

Node addresses are allocated dynamically and may differ on a fresh run. The allocator skips every address already recorded for NATS, platform nodes, and service VMs.

## Prerequisites

- WSL2 or Linux with `/dev/kvm`
- Firecracker and HAProxy installed
- `sudo`, `curl`, Python 3, OpenSSL, debootstrap, Rust, and Node.js
- Platform checkout available in WSL as `/mnt/d/dev/Wasm-Cloud-Platform`
- Application checkout available as `/mnt/d/dev/openid_connect_wasi`
- Sufficient free space on the Linux filesystem for Cargo targets and sparse VM images

Keep Cargo build output off `/mnt/d`:

```bash
export PATH=/opt/nodejs-22/bin:/home/djudicael/.cargo/bin:/usr/local/bin:/usr/bin:/bin
export CARGO_HOME=/home/djudicael/.cargo
export RUSTUP_HOME=/home/djudicael/.rustup
export CARGO_TARGET_DIR=/var/tmp/wcp-target-djudicael
export OIDC_CARGO_TARGET_DIR=/var/tmp/oidc-target-djudicael
cd /mnt/d/dev/Wasm-Cloud-Platform
```

## 1. Prepare VM assets

The successful rehearsal used persistent Linux-side assets under `/var/lib/wasm-cloud-platform-test`:

- Firecracker CI kernel `vmlinux-6.1.102`
- `wasm-node-rootfs.ext4`
- `nats-rootfs.ext4`
- `postgres-rootfs.ext4`

Build the root filesystems with the canonical scripts:

```bash
mkdir -p /var/tmp/wcp-vm-assets
OUTPUT_DIR=/var/tmp/wcp-vm-assets \
  CARGO_TARGET_DIR=/var/tmp/wcp-target-djudicael \
  scripts/vm/build-node-rootfs.sh
OUTPUT_DIR=/var/tmp/wcp-vm-assets scripts/vm/build-nats-rootfs.sh
OUTPUT_DIR=/var/tmp/wcp-vm-assets scripts/vm/build-postgres-rootfs.sh

sudo install -m 0644 /var/tmp/wcp-vm-assets/*.ext4 \
  /var/lib/wasm-cloud-platform-test/
```

Invoke the builders as the WSL user; they request `sudo` only for the image operations that need it. Running the whole node builder through `sudo` makes the shared Cargo target root-owned and later user builds fail on `.cargo-build-lock`.

The PostgreSQL image must include `postgresql17-contrib`; the application migration creates the `pgcrypto` extension.

## 2. Provision three platform nodes and NATS

For a fresh environment:

```bash
export VM_KERNEL_PATH=/var/lib/wasm-cloud-platform-test/vmlinux-6.1.102
export VM_NODE_ROOTFS=/var/lib/wasm-cloud-platform-test/wasm-node-rootfs.ext4
export VM_NATS_ROOTFS=/var/lib/wasm-cloud-platform-test/nats-rootfs.ext4

bash scripts/vm/provision-testbed.sh \
  --preset production-like \
  --nodes 3 \
  --name oidc-local \
  --state-file .oidc-vm-testbed-state.json \
  --front-door none
```

The OIDC-specific gateway is configured after both components are deployed, so this worked example initially uses `--front-door none`. Firecracker is launched in a detached session so VMs survive the end of a transient `wsl.exe` command.

Confirm status before adding services:

```bash
sudo -E /var/tmp/wcp-target-djudicael/debug/vm-testbed-cli \
  status --state-file .oidc-vm-testbed-state.json
```

## 3. Add PostgreSQL

```bash
bash scripts/vm/provision-postgres-service.sh \
  --state-file .oidc-vm-testbed-state.json \
  --rootfs /var/lib/wasm-cloud-platform-test/postgres-rootfs.ext4
```

The service is ready only after `172.20.0.20:5432` accepts TCP connections. It is recorded under `services` in the same state file and is therefore included in exact teardown.

## 4. Build, migrate, seed, deploy, and route OIDC Hub

```bash
export WASM_CTL_AUTH_TOKEN=local-test-write-token-change-me

bash scripts/vm/deploy-oidc-hub-test.sh \
  --app-dir /mnt/d/dev/openid_connect_wasi \
  --state-file .oidc-vm-testbed-state.json \
  --public-url http://localhost:8088
```

The script performs the complete workflow:

1. Runs locked frontend installation and production build with npm.
2. Builds `oidc-admin-wasi` and `openid-connect-wasi` for `wasm32-wasip2`.
3. Runs the application's migration system against the PostgreSQL microVM.
4. Idempotently seeds the `master` realm, admin account, test clients, and API key.
5. Uploads and deploys both Wasm components with separate internal Host routes.
6. Generates persistent local signing/encryption material beside the state file.
7. Configures HAProxy same-origin path routing and application-aware health pools.
8. Validates frontend HTML, database readiness, OIDC discovery issuer, SPA fallback, and the real realm login page.

HAProxy must not blindly round-robin every request across every node. The current scheduler has one serving placement per application; the generated gateway therefore health-checks frontend and backend routes independently and selects only nodes serving that component.

## 5. Manual validation

From Windows PowerShell:

```powershell
Invoke-WebRequest -UseBasicParsing http://localhost:8088/
Invoke-RestMethod http://localhost:8088/health/ready
Invoke-RestMethod http://localhost:8088/.well-known/openid-configuration
Invoke-WebRequest -UseBasicParsing http://localhost:8088/realms/master/login
Invoke-WebRequest -UseBasicParsing http://localhost:8404/stats

$loginBody = @{
  email = 'admin@example.com'
  password = 'Admin123'
  client_id = 'admin-ui'
} | ConvertTo-Json -Compress

$loginRequest = @{
  Method = 'Post'
  Uri = 'http://localhost:8088/oidc/login'
  ContentType = 'application/json'
  Body = $loginBody
}
$tokens = Invoke-RestMethod @loginRequest

if (-not $tokens.access_token -or -not $tokens.id_token) {
  throw 'OIDC login did not return both required tokens'
}
```

Expected results:

- frontend HTTP `200`, title `OpenID Connect Hub - Admin`
- readiness JSON has `status: ready` and `checks.database: ok`
- discovery issuer is exactly `http://localhost:8088`
- `master` realm login returns HTTP `200`
- HAProxy statistics returns HTTP `200`
- credential login returns both an access token and an ID token

`/oidc/login` is a `POST` API endpoint, not a browser page. Navigating directly to
that URL sends `GET` and should return HTTP `405`; use `/` or
`/realms/master/login` for interactive browser testing.

To prove that PostgreSQL is reachable from every platform node, bypass HAProxy
and query each node proxy with the backend host header:

```bash
curl -H 'Host: oidc-backend.internal' http://172.20.0.18:8080/health/ready
curl -H 'Host: oidc-backend.internal' http://172.20.0.19:8080/health/ready
curl -H 'Host: oidc-backend.internal' http://172.20.0.21:8080/health/ready
```

All three nodes returned `{"checks":{"database":"ok"},"status":"ready"}` in
the completed rehearsal. A failed database check is HTTP `503`; deployment
validation also parses the JSON and does not accept a generic `2xx` alone.

Browser validation should include sign-in, navigation, CRUD operations used by the admin UI, logout, an authorization-code flow, token exchange, and refresh/revocation behavior. The automated smoke test validates transport and service integration but cannot replace interactive product behavior checks.

## 6. Teardown — do not run until explicitly requested

```bash
bash scripts/vm/destroy-testbed.sh \
  --state-file .oidc-vm-testbed-state.json
```

This stops only the HAProxy PID, Firecracker PIDs, TAP devices, service VMs, bridge, state companions, and local OIDC keys recorded for this exact state file. The environment described in this document has not been destroyed.

## Observations and improvements

### Fixed during the rehearsal

- Firecracker API calls now use the configured Unix socket.
- Each VM receives a unique writable rootfs clone, MAC address, TAP device, node identity, and static address.
- Firecracker and HAProxy processes use detached sessions and survive WSL command exit.
- PostgreSQL rootfs ownership and `pgcrypto` packaging were corrected.
- WASI HTTP hosting now uses Wasmtime's asynchronous linker and instantiation APIs, eliminating nested-Tokio-runtime panics.
- The artifact router's Axum body limit now matches its intended 100 MiB artifact guard; the 5.8 MiB backend previously hit Axum's 2 MiB default.
- Scaling skips addresses reserved by service VMs and persists every successful scale step.
- Status checks recognize root-owned processes when invoked by an unprivileged deployment user.
- OIDC routing uses independent frontend/backend health pools rather than sending traffic to non-serving placements.
- E2E binary and WASI artifact discovery now honors absolute or relative `CARGO_TARGET_DIR` values.
- Hot-reload E2E admin and artifact ports no longer overlap. Run that resource-heavy test binary with its documented `--test-threads=1` under WSL to avoid concurrent NATS/node startup exhaustion.
- Per-instance outbound TCP reservations are released when sockets or stores are dropped. Repeated readiness probes no longer leak the shared connection counter until the runtime denies PostgreSQL with `access-denied`.
- Late-joining nodes acknowledge historical deployment events that lack an audience-bound artifact authorization instead of repeatedly NAKing an event that can never succeed.
- JetStream item errors no longer silently terminate a node's durable control-event loop, so later deploy and route events continue to converge.
- Peer-owned `127.0.0.1` instance announcements are not registered as local upstreams. This prevents identical loopback ports on different VMs from cross-wiring HTTP/1 and h2c runtimes.
- The detached testbed CLI can restart one exact recorded platform node from the current rootfs while preserving its identity, IP, NATS VM, PostgreSQL VM, HAProxy, and testbed state.
- The OIDC smoke test now validates the readiness payload (`status=ready`, `checks.database=ok`) and uses normal runtime policy limits; the temporary oversized outbound-connection allowance is no longer needed.
- Password login performs Argon2 verification inside Wasm and needs a larger fuel quota than the generic 500-million default. The OIDC deployment uses the platform's 10-billion maximum and includes that quota in the deployment version identity so a limit change cannot silently reuse an instance created with an older quota.
- Readiness and the HTML login page alone are insufficient authentication checks. The deployment gate now submits the seeded credentials to `POST /oidc/login` and requires both an access token and ID token before reporting the environment ready.
- The runtime's former 10-tick epoch deadline equaled roughly 100 ms and interrupted legitimate Argon2/JWT login work even when fuel remained. Production runtime stores now use a 3,000-tick (roughly 30-second) coarse deadline; fuel still provides the instruction-level CPU bound.
- A 1 GiB platform guest crossed the default free-memory pressure threshold after loading both the backend and admin UI, causing intentional 503 backpressure. The `production-like` preset now defaults to 2 GiB per platform node. With that allocation, all three nodes independently returned database-ready and successful token responses, followed by six successful logins through HAProxy.

### Root-cause analysis of the login 502

The original `POST /oidc/login` failure was not a PostgreSQL or HAProxy routing
failure. It was caused by two independent Wasm execution limits, encountered in
sequence:

| Stage | Evidence | Conclusion | Correction |
|---|---|---|---|
| Database connection | Every node established TCP to `172.20.0.20:5432`, authenticated, began the transaction, and later completed `SELECT 1`. | PostgreSQL networking, credentials, and WASI socket policy were working. | No database-network workaround was required. |
| Initial login | Argon2 password verification exhausted the generic 500-million fuel quota. | The login request itself needed more instructions than the default allowance. | The OIDC backend receives 10 billion fuel units per request. |
| Login after increasing fuel | The request progressed further but Wasmtime interrupted it before completion. | The old 10-tick epoch deadline was only about 100 ms and remained too short regardless of available fuel. | Production stores use 3,000 ticks, approximately 30 seconds with the current 10 ms epoch ticker. |
| Restart with corrected runtime | Both applications loaded into a 1 GiB node, available pages fell to 54,205, below the 65,536-page (about 256 MiB) pressure threshold, and the proxy returned 503 backpressure. | This was a separate node-capacity problem, not the cause of the original login 502. | The production-like test preset now allocates 2 GiB per node. |

Fuel is allocated per HTTP request. The runtime creates a fresh Wasmtime `Store`
and configures its fuel before handling each request. HAProxy health checks therefore
cannot consume fuel belonging to a later login request.

HAProxy did contribute to the observed startup memory pressure: it checks both the
frontend `/` and backend `/health/ready` on each node every two seconds. Those checks
promptly cold-started both Wasm applications after a rolling restart. On a 1 GiB
guest, the combined resident memory crossed the pressure threshold. The checks were
the trigger that exposed undersizing, not a cumulative fuel or memory leak. The same
checks remain enabled on the 2 GiB nodes without causing backpressure.

The final proof used more than a readiness response:

- database readiness returned `database: ok` through each of the three node proxies;
- a real credential login succeeded directly against each node;
- six consecutive credential logins succeeded through HAProxy;
- every successful login contained both `access_token` and `id_token`; and
- the frontend remained HTTP `200` through the same front door.

### Production guidance derived from the incident

- Treat fuel and epoch deadlines as separate controls. Fuel bounds executed Wasm
  instructions; the epoch deadline bounds elapsed time. Increasing only one can
  leave the other as the effective failure limit.
- Do not copy the rehearsal's 10-billion fuel and 30-second deadline into production
  without load testing. Measure successful Argon2 login fuel and latency at p95/p99
  under CPU contention, then select explicit headroom while keeping a finite denial-of-service bound.
- Rate-limit authentication independently from generic HTTP traffic. Argon2 is
  intentionally expensive, so fuel alone is not sufficient protection against a
  distributed login flood.
- Benchmark the configured Argon2 memory, iterations, and parallelism inside the
  actual microVM CPU and memory limits. Do not weaken password hashing merely to fit
  an undersized runtime quota.
- Capacity-plan for the steady-state resident set of every colocated Wasm component,
  compilation/cache overhead, concurrent requests, and the platform process. Keep
  normal available memory comfortably above the 256 MiB backpressure threshold; 2
  GiB is the validated local minimum for these two components, not a universal
  production recommendation.
- Account for health-check fan-out. With three nodes and two checks every two seconds,
  HAProxy generates an average of three application requests per second before user
  traffic. At larger node counts, use a cheap liveness check and a cached/debounced
  dependency readiness result instead of querying PostgreSQL on every load-balancer check.
- Consider HAProxy check spreading and a startup grace period to avoid simultaneous
  cold starts after a rolling restart. Keep `fall` and `rise` thresholds so a single
  slow response does not flap a backend.
- Monitor fuel exhaustion, epoch interruption, cold-start duration, available pages,
  backpressure transitions, Argon2 latency, database connection/query latency, and
  HAProxy backend status as distinct metrics. A generic 502 alone is not actionable.
- Alert on readiness payload content, not only HTTP status. Specifically require
  `status=ready` and `checks.database=ok`, and keep a separate synthetic login that
  verifies token issuance without running at load-balancer-check frequency.
- Include runtime limits in release identity or deployment configuration reconciliation.
  A changed fuel quota must restart existing application instances; a changed runtime
  deadline requires a rolling node-binary restart.
- Roll platform nodes one at a time and require application-level readiness before
  proceeding. Keep PostgreSQL, NATS, and HAProxy running during a node-only rollout,
  and exercise the login path after every complete rollout.

For future incidents, classify the failure before changing limits:

| Signal | Likely area |
|---|---|
| No TCP connection attempt to PostgreSQL | application configuration, WASI socket policy, DNS, or routing |
| TCP succeeds but authentication fails | credentials, PostgreSQL roles, or secret delivery |
| Database query succeeds, followed by an out-of-fuel trap | per-request fuel quota or unexpectedly expensive application work |
| Database query succeeds, followed by epoch interruption | elapsed-time deadline, CPU contention, or blocking work |
| Proxy reports `node at capacity` and memory-pressure events | guest sizing, resident application set, concurrency, or a memory leak |
| Only one HAProxy backend fails while direct requests to others pass | node-specific placement, bootstrap, artifact, or runtime state |
| Direct requests pass on all nodes but the front door fails | HAProxy routing, host/path ACLs, health state, or front-door timeout |

### Remaining production gaps

- The local front door is HTTP only. Add real certificates, redirect policy, secure cookies, and trusted proxy boundaries for production.
- NATS is a single microVM. A production control plane needs a persistent, authenticated, highly available NATS design.
- PostgreSQL is a single ephemeral test VM with local credentials. Production needs external secret management, encrypted connections, durable volumes, backups, restore drills, monitoring, and HA/failover.
- This rehearsal deploys the application to every active platform node. Production still needs an explicit desired-replica/placement policy, disruption budgets, and tested rescheduling semantics rather than treating membership-wide deployment as a complete HA strategy.
- Node rootfs health is degraded by the small test image's disk-space threshold. Increase the rootfs/data disk and align the health threshold before using disk health as a release gate.
- Use a pinned, checksummed Firecracker-compatible kernel artifact in automated image preparation. The earlier locally built minimal kernel did not boot reliably.
- Add observability for placement changes, artifact transfer, route convergence, PostgreSQL latency, and HAProxy backend state.
- Add automated browser tests for login and the admin UI, plus OIDC conformance and failover tests.

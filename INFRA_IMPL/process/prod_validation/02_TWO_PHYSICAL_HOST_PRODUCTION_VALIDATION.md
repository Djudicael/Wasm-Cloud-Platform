# Two-physical-host production validation plan

## Purpose

Use this plan after the single-host microVM plan passes. It validates failures
that cannot be created honestly when every microVM shares one computer: complete
host shutdown, host-kernel failure, physical disk loss, network-interface loss,
and cross-host routing for ingress, control-plane, stateful services, telemetry,
and explicit external application endpoints. The `.internal` application mesh
remains node-local; this plan must not introduce cross-host mesh forwarding or
identity.

The two hosts may be:

- two Linux computers;
- two Windows computers with WSL2 and working `/dev/kvm`;
- one WSL2 computer and one Intel Mac running Linux directly;
- one WSL2 computer and one Intel Mac running an x86_64 Linux VM with nested KVM;
  or
- two temporary Linux VPS/bare-metal instances exposing KVM where Firecracker is
  required.

If nested KVM is unavailable, a Linux VM may run `wasm-node` directly and still
serve as a second physical failure domain. Record this topology difference.

## What two hosts prove

This plan can validate:

- survival of either physical computer being powered off;
- independent host kernels, memory, disks, and network interfaces;
- routed platform, NATS, PostgreSQL, proxy, and telemetry traffic between hosts;
- application continuity and N-1 capacity during complete host loss;
- redundant load balancer behavior;
- primary/replica database behavior with manual or properly arbitrated failover;
- telemetry continuity when one collector/host disappears; and
- eBPF behavior on two real kernels and hardware configurations.

For application-to-application `.internal` calls, the two-host proof is that
each host independently deploys the complete `every_node` dependency closure,
resolves to its own loopback gateway, and fails locally when a dependency is
absent. Cross-host mesh identity is out of scope by design.

Two physical hosts alone do **not** prove:

- three-way consensus quorum after either host fails;
- absence of split-brain during automatic two-member database failover;
- availability-zone independence when both hosts share one building, switch,
      router, power circuit, or ISP;
- provider-managed load-balancer, disk, PKI, KMS, or secret-manager guarantees; or
- multi-region behavior.

Use a third independent host or small VPS for a third NATS voting member and any
required database failover witness. Without it, record quorum and automatic
failover as `NOT VALIDATED` or block the corresponding production claim.

## Mandatory automation gap before execution

The current `scripts/vm/` workflow is local-host oriented. Running
`provision-testbed.sh` independently on two computers would create two local NATS
services, potentially overlapping guest subnets, and separate state files; it
would **not** automatically create one distributed platform.

Before executing this plan, implement and review multi-host orchestration that:

- consumes an explicit inventory rather than discovering hosts by pattern;
- assigns unique host, platform-node, TAP, bridge, subnet, and advertised addresses;
- supports an external or explicitly clustered NATS topology;
- supports PostgreSQL primary/replica or external database endpoints;
- configures cross-host routes and firewalls idempotently;
- creates HAProxy instances with application-aware health pools;
- records one coordinator manifest plus one exact local state file per host;
- executes remote operations over authenticated SSH;
- refuses ambiguous, duplicate, unroutable, or overlapping addresses;
- supports partial retry without duplicating streams, routes, or services; and
- destroys only resources recorded for the selected run on the selected host.

Do not work around this gate with unrecorded manual networking or broad process
cleanup. The multi-host automation must be tested in a disposable environment
before it is trusted for this rehearsal.

## Reference topology

With two physical hosts and an optional independent witness:

```text
Host A                                  Host B
├── platform-node-1                     ├── platform-node-2
├── platform-node-3                     ├── platform-node-4
├── HAProxy-A                           ├── HAProxy-B
├── NATS-1                              ├── NATS-2
├── PostgreSQL primary                  ├── PostgreSQL replica
└── telemetry agent A                   └── telemetry agent B

Independent host/VPS C (recommended)
├── NATS-3 voting member
├── database failover witness when required
└── external synthetic probe / optional telemetry gateway
```

Distribute platform nodes so each physical host can carry the required traffic
after the other fails. More microVMs on the same host improve process-level
redundancy but do not create more physical failure domains.

Do not place both the production database primary and its only usable backup on
one host. Do not place two of three NATS voting members on one of only two hosts
and claim either-host survival; loss of the host holding two voters loses quorum.

## Inventory and evidence record

Create a reviewed inventory before provisioning:

| Field | Host A | Host B | Optional Host C |
|---|---|---|---|
| Physical location/failure domain | | | |
| Hostname and stable LAN/VPN address | | | |
| OS and kernel | | | |
| CPU architecture and virtualization | | | |
| CPU, memory, and disk allocation | | | |
| KVM `/dev/kvm` status | | | |
| Platform-node IDs | | | |
| Guest subnet | | | |
| NATS client/route/monitor addresses | | | |
| PostgreSQL role/address | | | |
| HAProxy address | | | |
| Telemetry endpoint | | | |
| Local state-file path | | | |

Use non-overlapping networks, for example:

```text
Physical LAN/VPN: 192.0.2.0/24       # replace with the actual private network
Host A guests:     172.28.1.0/24
Host B guests:     172.28.2.0/24
Host C guests:     172.28.3.0/24
```

The example `192.0.2.0/24` block is documentation space, not a literal deployment
network. Select actual private ranges after checking LAN, VPN, WSL, container, and
corporate routes for conflicts.

For the run, record:

```text
Run ID:
Operators and rollback authority:
Git commit and release checksums:
Inventory revision/checksum:
Coordinator manifest:
Host-local state files:
Application commit/checksums:
SLO, RTO, RPO, and N-1 capacity target:
Test start/end:
Result: PASS / FAIL / EXCEPTION
Evidence location:
```

## Phase 0: prerequisite from the single-host plan

- [ ] The exact release has passed
      `01_SINGLE_HOST_MICROVM_PRODUCTION_VALIDATION.md`.
- [ ] Open findings and exceptions from that run are reviewed.
- [ ] The same application artifacts, database migrations, HAProxy routes,
      synthetic OIDC journey, dashboards, alert rules, and load profiles are used.
- [ ] No production secrets or customer data are used.
- [ ] A named operator can halt fault injection and restore routing immediately.
- [ ] Every destructive operation requires a resolved host and a recorded resource
      identifier from this run.

## Phase 1: physical-host and KVM preflight

On both hosts, record and compare:

```bash
hostnamectl
uname -a
uname -m
lscpu
free -h
df -h
ip -brief address
ip route
timedatectl status
test -r /dev/kvm && test -w /dev/kvm && echo "KVM available"
```

- [ ] Both clocks synchronize to reliable time sources.
- [ ] Both hosts run supported x86_64 Linux environments.
- [ ] KVM is available wherever Firecracker will run.
- [ ] Host firewalls default-deny unsolicited traffic and allow only the documented
      source/destination/port matrix.
- [ ] Host A and Host B use independent disks and do not store all backups on one
      shared physical device.
- [ ] Host capacity is reserved so background desktop workloads do not invalidate
      measurements.
- [ ] On WSL2, guest/service endpoints are advertised using addresses routable from
      the other physical host, not an inaccessible WSL NAT address.
- [ ] On an Intel Mac, Firecracker runs only inside Linux with working nested KVM
      or Linux booted directly; macOS itself is not treated as a KVM host.

## Phase 2: cross-host network construction

- [ ] Assign non-overlapping guest subnets and unique TAP/bridge names per host.
- [ ] Establish explicit routes between the two guest subnets.
- [ ] Verify reverse-path routing; do not rely on one-way host port forwarding.
- [ ] Configure NAT only at deliberate egress boundaries.
- [ ] Ensure NATS advertises addresses reachable from all platform nodes.
- [ ] Ensure PostgreSQL and internal gateway addresses are reachable only from
      intended networks.
- [ ] Firewall policy prevents `.internal` gateway traffic from leaving its
      platform node; there is no remote-node discovery or fallback route.
- [ ] Ensure administration and artifact endpoints are never exposed to the public
      LAN without TLS and authorization.
- [ ] Verify MTU across LAN/VPN, WSL, bridge, TAP, and guest interfaces.
- [ ] Verify DNS and certificate names resolve identically from both hosts.

Capture successful bidirectional tests for every required flow:

| Source | Destination | Purpose |
|---|---|---|
| Platform nodes A/B | NATS members | Client/control traffic |
| NATS members | Other NATS members | Cluster routes and JetStream replication |
| Platform nodes A/B | PostgreSQL | Application database traffic |
| HAProxy A/B | Platform proxies A/B | North-south routing |
| Platform nodes | Other platform nodes | Explicit control/artifact flows only; never `.internal` mesh traffic |
| Prometheus/collectors | All monitored targets | Metrics/logs/traces |
| Operators | Admin endpoints | Authenticated management only |

## Phase 3: provision and establish distributed state

- [ ] Provision from the reviewed inventory using the multi-host orchestration.
- [ ] Persist a separate exact local state file on each physical host.
- [ ] Persist a coordinator manifest mapping every service and microVM to its host.
- [ ] Verify unique platform node IDs and advertised addresses.
- [ ] Verify one shared platform control topology, not two isolated local testbeds.
- [ ] Verify NATS cluster membership, JetStream replicas, leader placement, storage,
      TLS identities, and subject permissions.
- [ ] Verify PostgreSQL role, replication state, connection policy, and failover
      fencing/witness behavior.
- [ ] Verify HAProxy A and B use the same generated application routing intent but
      independent runtime state.
- [ ] Verify telemetry from both hosts reaches the shared dashboards with distinct
      host and node labels.

If no third independent voter exists:

- [ ] Disable unsafe automatic database promotion or document the fencing method.
- [ ] Record NATS quorum survival after arbitrary physical-host loss as
      `NOT VALIDATED` unless the actual placement preserves a majority.

## Phase 4: deploy and verify the representative application

- [ ] Deploy the same OpenID Connect WASI Hub artifacts validated on one host.
- [ ] Run migrations through the designated single migration job/owner.
- [ ] Verify platform placement spans both physical hosts.
- [ ] Verify HAProxy A and HAProxy B can independently serve the complete public
      application origin.
- [ ] Run OIDC discovery, login, callback, authenticated access, session refresh
      where supported, and logout through each front door.
- [ ] Verify application-to-PostgreSQL traffic crosses hosts in at least one test.
- [ ] Verify application routing crosses hosts in at least one test.
- [ ] Verify no node-local hostname, path, credential, or address has leaked into
      promoted application configuration.

## Phase 5: redundant front door

- [ ] Put a stable test address in front of HAProxy A and HAProxy B using the
      intended self-managed mechanism, DNS strategy, or external test load balancer.
- [ ] Verify health checks are application-aware.
- [ ] Verify client IP and forwarded headers are accepted only from trusted proxies.
- [ ] Verify TLS certificate selection, protocol policy, request/body limits,
      timeouts, connection draining, and long-lived requests.
- [ ] Stop HAProxy A and confirm new traffic reaches HAProxy B.
- [ ] Restore A and verify it rejoins without interrupting existing traffic.

If the intended production load balancer is provider-managed, this phase validates
the platform contract but not provider behavior; repeat the LB tests in provider
staging.

## Phase 6: complete Host A failure

Before the test, prove Host B alone has the declared N-1 capacity and identify
which NATS/PostgreSQL functions are expected to survive.

- [ ] Start continuous synthetic OIDC journeys and representative load from a
      third location or the host that will remain alive.
- [ ] Capture baseline metrics, logs, traces, NATS state, PostgreSQL state, and
      HAProxy backend state.
- [ ] Power off Host A or terminate its complete Linux/WSL environment. Do not
      merely stop one microVM.
- [ ] Measure failure detection, routing convergence, error rate, p99 latency,
      application availability, NATS quorum, and database availability.
- [ ] Confirm no manual DNS or client reconfiguration is needed unless explicitly
      part of the production design.
- [ ] Keep Host A offline for longer than all health, lease, session, and
      reconciliation timeouts.
- [ ] Restore Host A and verify membership, routes, consumers, deployments,
      database replication, and telemetry converge without duplicates or stale
      state.
- [ ] Verify recovery traffic does not overload PostgreSQL, NATS, or Host B.

## Phase 7: complete Host B failure

Repeat Phase 6 with Host B as the failed host. A design does not pass arbitrary
single-host failure merely because it survives loss of the less important host.

- [ ] Verify Host A alone meets the same declared service and capacity target.
- [ ] Verify loss of the PostgreSQL replica differs safely from loss of the primary.
- [ ] Verify NATS voting-member placement behaves as predicted.
- [ ] Verify HAProxy and telemetry failover do not depend on Host B.
- [ ] Restore Host B and verify clean convergence.

## Phase 8: cross-host network partition

A partition is different from a powered-off host because both sides may remain
active.

- [ ] Block Host A-to-B traffic while leaving client traffic to both sides active.
- [ ] Verify NATS minority behavior and prevent split-brain writes.
- [ ] Verify PostgreSQL promotion/fencing behavior matches the documented design.
- [ ] Verify platform nodes do not advertise unreachable application instances.
- [ ] Verify HAProxy removes only the actually unreachable application paths.
- [ ] Verify administrative operations fail safely when consensus is unavailable.
- [ ] Restore the network and verify convergence without duplicate migrations,
      duplicate deployment actions, or lost desired state.
- [ ] Repeat with asymmetric loss, latency, packet loss, and restricted bandwidth.

Never improvise automatic PostgreSQL promotion in a two-member topology. Require a
tested witness/fencing mechanism or use explicit manual promotion.

## Phase 9: independent storage and recovery

- [ ] Abruptly terminate the host containing the PostgreSQL primary during writes.
- [ ] Execute only the documented failover procedure.
- [ ] Measure committed-data loss and recovery time against RPO/RTO.
- [ ] Prevent the former primary from accepting writes until it is safely
      reintegrated or rebuilt.
- [ ] Restore a backup onto the other physical host and run database integrity,
      readiness, and complete OIDC tests.
- [ ] Demonstrate that backup encryption keys and backup data are not stored only
      on the failed host.
- [ ] Test loss of one NATS storage directory/member and its controlled rebuild.

## Phase 10: telemetry survival

- [ ] Maintain one telemetry agent per host and redundant or recoverable central
      storage/gateways as required by the design.
- [ ] Power off the host containing one collector and verify the other host's
      platform traffic remains unaffected.
- [ ] Confirm bounded local buffering during telemetry interruption.
- [ ] Verify alerts arrive from outside the failed host.
- [ ] Confirm dashboards distinguish host failure, node failure, dependency
      failure, network partition, and telemetry-pipeline failure.
- [ ] Confirm audit records generated before the failure exist off-host.
- [ ] Follow one distributed request across nodes and physical hosts using trace
      and request IDs.

## Phase 11: eBPF comparison

- [ ] Record both physical CPUs, host/guest kernels, microcode, BTF, cgroup mode,
      security modules, and eBPF feature set.
- [ ] Run fallback mode on both hosts and capture a load-test baseline.
- [ ] Enable eBPF on one canary node while keeping an equivalent control node.
- [ ] Compare CPU, memory, p50/p95/p99 latency, event loss, and backpressure.
- [ ] Confirm cgroup scoping excludes the other host's and unrelated local
      workloads.
- [ ] Deny or remove eBPF capability and verify safe fallback.
- [ ] Repeat on the second host to detect kernel/hardware-specific behavior.
- [ ] Record these results as evidence for only the tested kernels and hardware.

## Phase 12: N-1 load, upgrade, and rollback

- [ ] Establish maximum sustainable load for the two-host test configuration.
- [ ] Select a production target below the capacity of either surviving host, with
      explicit safety headroom.
- [ ] Sustain the target while Host A is offline, then while Host B is offline.
- [ ] Perform a rolling platform upgrade without losing all replicas of an
      application or quorum service at once.
- [ ] Perform an application upgrade with compatible migrations under load.
- [ ] Trigger an automatic or operator-controlled rollback based on an SLO breach.
- [ ] Verify the preserved last-known-good binary, configuration, artifacts, and
      schema compatibility actually restore service.

Two-host measurements apply only to the tested hardware, network, and storage.
They do not establish a provider VPS capacity figure unless those exact VPS types
and storage services were used.

## Phase 13: acceptance decision

The two-host validation passes only when:

- [ ] Either complete physical host can fail without violating the declared
      availability and N-1 capacity target.
- [ ] Cross-host partition behavior cannot produce unsafe concurrent authority or
      database writes.
- [ ] Routing, platform state, NATS, PostgreSQL, and telemetry recover predictably.
- [ ] Backups restore on infrastructure independent from the failed host.
- [ ] Upgrade and rollback succeed during representative traffic.
- [ ] Every result is supported by timestamped metrics, logs, traces, state output,
      and operator notes.
- [ ] Quorum, provider service, zone, region, and capacity limitations are recorded
      honestly as pass, fail, exception, or not validated.

Required final summary:

```text
Host A loss result and recovery time:
Host B loss result and recovery time:
Partition result:
NATS quorum result:
PostgreSQL RPO/RTO and fencing result:
N-1 capacity result:
Load-balancer failover result:
Observability survival result:
eBPF comparison result:
Backup restore result:
Upgrade and rollback result:
Unvalidated production-provider properties:
Exceptions with owner/approver/expiry:
Final decision: PASS / FAIL / CONDITIONAL
```

## Phase 14: coordinated teardown

Teardown requires explicit user authorization after interactive testing is complete.

- [ ] Stop external traffic and synthetic tests.
- [ ] Capture final state and evidence before deleting anything.
- [ ] Resolve the coordinator manifest and both host-local state files to explicit
      absolute paths.
- [ ] On each host, destroy only the local resources recorded for this run.
- [ ] Remove only routes and firewall rules tagged or recorded for this run.
- [ ] Confirm NATS, PostgreSQL, HAProxy, collectors, Firecracker PIDs, TAP devices,
      and bridges from unrelated environments remain untouched.
- [ ] Remove temporary credentials and revoke temporary certificates.
- [ ] Preserve the redacted evidence package and final decision record.

Do not use broad process-name matching, subnet-wide deletion, recursive deletion of
workspace roots, or cleanup based on unresolved variables.

## Next validation tier

After this plan passes, repeat only the infrastructure-dependent tests on the
actual intended substrate:

- three independent voting/failure domains;
- provider load balancer and DNS;
- production storage class and snapshot service;
- non-production namespace in the real PKI/KMS/secret manager;
- production-sized load and soak test; and
- a small controlled canary using the exact production kernel and hardware.

This final tier should reuse the same artifacts and automated acceptance suite,
not introduce a different hand-built deployment.

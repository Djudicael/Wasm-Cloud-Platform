# P10-05 node-local mesh production-contract evidence

Result: **PASS within the single-host, three-node Firecracker boundary**.

The run used the current node image schema 12 and the literal
`mesh-role-target-a1316b85-e7d1.validation.internal:9080` URL. It did not replace
DNS with a loopback URL or a test-only `Host` override.

Validated on each of `local-test-node-0`, `local-test-node-1`, and
`local-test-node-2`:

- node-local `.internal` DNS resolution and gateway routing;
- eBPF source-port/TID application identity under simultaneous sustained load;
- 96/96 successful requests at concurrency 8 per node (288/288 total);
- realm-role, client-role, missing/malformed authorization, forged-header, and
  cross-namespace denial behavior (24/24 checks);
- `every_node` placement with a declared same-namespace local dependency;
- target removal produced HTTP 502 on all three nodes without a remote-node
  lookup or retained-artifact cold start;
- target redeployment restored HTTP 200 on all three nodes;
- CLI-style WASI services remained alive beyond the former 30-second epoch
  lifetime boundary.

`RESULT_SUMMARY.json` is the machine-readable result and `SHA256SUMS` protects
that summary. It contains artifact hashes and status results, but no bearer
tokens, signing keys, passwords, or response bodies.

## Architecture decision

The internal mesh is node-local and shared-nothing by design. Cross-host mesh
discovery, forwarding, and workload identity are explicitly out of scope; they
must not be described as an unfinished platform feature. An application that
intentionally calls a remote service uses an explicit external URL through the
separately secured ingress/API-gateway path.

## Production qualification still required

This evidence does not replace a run from the signed production artifact. Repeat
the validator on every production node image/kernel combination and retain:

- resolver and eBPF-required configuration;
- node/image digests and kernel/BTF identity;
- load profile, identity records, and dependency-failure statuses;
- alerts for eBPF unavailable, unresolved workload identity, local dependency
  unavailable, and sustained 502 responses.

When active connection limits are enforced, eBPF must be mandatory because TCP
close events release the per-instance outbound reservation.

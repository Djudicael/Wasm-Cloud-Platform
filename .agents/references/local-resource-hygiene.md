# Local build and test storage

Read this before substantial local builds, container-backed tests, model
downloads, or changes to a development controller/test fixture. These are
engineering and operating requirements, not claims that a CLI already
implements a guard or cleanup command. Extend the existing Rust controller
when automation is needed; do not introduce a second launcher.

## Space and build preflight

- Resolve the effective Cargo target directory (environment, Cargo config and
  workspace defaults), pinned toolchain, profiles and targets before building.
  Reuse the project's established Linux target directory under WSL; do not
  invent a fresh directory for every command. Follow AGENTS.md when it pins a
  path. Record exceptional cold-build/benchmark directories and their cleanup.
- Measure free bytes on the actual build/data filesystem and on the Windows
  volume backing the WSL VHDX. A checkout on D: and a target under Linux /tmp
  can still consume C:. WSL's large virtual capacity is not host free space.
  Include downloads, temporary copies, old artifacts and the planned validation
  matrix in the budget. Keep an explicit safety reserve; if the task has no
  measured budget, 20 GiB of host free space is an initial warning floor, not
  proof that a cold build fits. Defer new large writes when the expected peak
  plus reserve does not fit. Report the measured shortage instead of building
  until the disk is full or deleting unrelated data.
- Use targeted, sequential size measurements with bounded output. Stop an
  obsolete scan before starting another. Check permissions and Windows/WSL
  errors if scans fail or the editor crashes; do not assume an OOM or restart
  WSL to work around it. Do not dump container environments or credentials.

## Disposable tests versus persistent environments

Give each resource an explicit lifecycle: disposable test run, persistent
checkout stack, or externally owned/shared resource. Container stopped,
anonymous, unlabelled, or dangling does not by itself establish ownership or
permission to erase its contents.

For new or changed container-backed fixtures:

1. Assign project, canonical-checkout identity and a unique run identity to
   disposable containers and explicitly created disposable volumes. Use labels
   plus an exact resource ledger outside the disposable resources. Record IDs
   as soon as creation succeeds, including partial startup. Do not put secrets
   in labels, the ledger, or cleanup receipts.
2. Inspect actual mounts after container creation. Images can declare VOLUME
   and create anonymous volumes even without an explicit application mount.
   Record those volume identities too. Container labels do not automatically
   label volumes; supply volume labels at creation when creating them explicitly.
3. Give teardown an explicit owner covering success, assertion/setup failure,
   cancellation and partial initialization. A static OnceCell/Lazy container
   is not a process-exit cleanup guarantee: static destructors do not run.
   RAII or an EXIT trap also cannot guarantee cleanup after SIGKILL or a host
   crash. Use the existing test/controller lifecycle with a durable cleanup
   ledger and a recovery path, not a global process-exit assumption.
4. Stop/remove only the exact owned test containers. With the installed Podman
   version verified, `podman rm --volumes EXACT_CONTAINER_ID` removes its
   anonymous volumes; explicitly created disposable named volumes need their
   own exact-ID/name cleanup. Recheck mounts and ownership before removal and
   refuse volumes referenced by another container. Do not use force to bypass
   an ownership/reference conflict. Report retained resources and cleanup errors
   without hiding the test's original failure.
5. Recover interrupted runs only from recorded checkout/run ownership, after
   verifying the original owner is no longer active (PID plus start time and
   any controller lock/lease). Preserve parallel runs. Image-wide filters,
   generated container names, database names or a guessed library label alone
   are not a safe recovery selector. Verify that intended labels were actually
   applied; do not assume rustainers adds a particular label.
6. Verify that disposable container AND volume identities are gone afterwards;
   successful container removal alone is insufficient. Retain a before/after
   receipt of IDs, reclaimed space and any pending cleanup. When implementing
   fixture/controller lifecycle changes, exercise repeated runs, startup/test
   failure, cancellation, stale-run recovery and survival of an unrelated
   labelled fixture. Do not claim these checks passed without executing them.

Persistent PostgreSQL/RustFS/NATS volumes, document objects, credentials and
signing keys survive ordinary stop/smoke/restart. Delete them only for an
explicitly authorized destroy, using the existing controller and its exact
ownership checks. Do not add blanket volume removal to a shared stop routine.
Never use global system prune, volume prune, rm --all, wildcards, broad process
kills or deletion of Podman's storage directory as routine test cleanup. A
separately requested historical cleanup requires inventory and scoped selection,
including protection of persistent and externally owned data.

## Cargo, inference and WSL retention

- `cargo clean` only cleans its effective target directory. Compare that path
  with the recorded build paths before claiming old artifacts were removed.
  Clean only identified idle build outputs, after checking running builds and
  services that use them. Never delete all /tmp, /var/tmp, .cargo or .rustup.
- Reuse unchanged artifacts and an already healthy compatible development
  stack. Rebuild/redeploy changed components and their real dependents; do not
  rebuild an unchanged platform or OIDC Hub on every agentic test. Preserve the
  project's required checks and pinned toolchain/feature/backend matrix.
- Avoid routine clean-and-rebuild cycles. For a deliberately chosen local
  development profile, reducing dependency debug information can reduce disk
  use; disabling incremental compilation trades rebuild speed for space.
  Preserve diagnostic/sanitizer/benchmark requirements and do not silently
  change global Cargo profiles or required release flags to recover space.
- Treat verified model weights and native SDK installations as retained inputs,
  not disposable compiler caches. Bound compilation/download caches by an
  explicit byte/entry/age policy; evict only inactive owned entries. Preserve
  model revisions, active worker artifacts, credentials and validation evidence.
- Distinguish Linux free space, VHDX file length and host physical free space
  when reporting reclaimed storage. Deleting Linux files may not shrink the
  VHDX. Compaction is a separate explicitly authorized operation requiring
  orderly WSL shutdown and possibly Windows administrator confirmation; do not
  stop the user's sessions or compact a mounted writable disk automatically.
  An elevation cancellation is not successful compaction. Measure host free
  space after the operation before claiming a host-space gain.

Finish substantial validation with a brief resource receipt: effective target
path, disposable resources created/removed/pending, persistent resources kept,
and measured free space before/after. Keep the receipt free of credentials and
private document/model contents. If automatic guards or teardown are missing,
identify that implementation gap rather than presenting these instructions as
an already enforced runtime guarantee.

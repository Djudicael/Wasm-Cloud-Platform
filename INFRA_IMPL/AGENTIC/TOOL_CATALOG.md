# Agent tool catalogue for bootstrapping platform-hosted harnesses

Date: 2026-10-28 (extended 2026-10-29)  
Status: Proposed target catalogue; existing capabilities and remaining contracts are distinguished below
Companions: [INVESTIGATION.md](INVESTIGATION.md), [PLATFORM_HOSTED_HARNESS.md](PLATFORM_HOSTED_HARNESS.md), [PLATFORM_MODIFICATIONS.md](PLATFORM_MODIFICATIONS.md)

> Repository decision (2026-10-28): agent applications, harnesses, tools,
> knowledge/skills, migrations, and agent rehearsal scripts are developed in
> `D:/dev/wasmcloud_platform_harness`. This repository supplies generic execution
> capabilities. The coordinator owns durable transitions; harnesses return
> proposals without direct database access. This decision supersedes examples
> below that assign run-state writes or database credentials to a harness.
> See [platform progress](PLATFORM_MODIFICATIONS.md#repository-boundary-and-implementation-progress).

## Implementation alignment (2026-10-30)

The harness repository already implements durable coordination, bounded loop
harnesses, a worker tool broker, run-bound document reads and literal search,
approval-gated immutable results, and an operator CLI including run listing and
safe error inspection. These are capabilities, not implementations of every
similarly named catalogue contract. Preserve deployed tool names and pins.

The harness also implements bounded single-file V0 validation as `patch-validate-v1`, approved immutable patch artifacts and CLI `patch-export`. Its `docs/patch-artifacts.md` records completed real OpenRouter generation through `patch-edit-v1`, approval and immutable export, along with earlier failures and the separate deterministic rehearsal. The structured editor preserves source indentation and feeds the unchanged strict validator.

The current workspace slice follows the harness `docs/coding-agent-plan.md`:
immutable archive ingestion, `workspace.list`, `workspace.read`, and
`workspace.grep`, followed by a CLI-driven read-only repository analysis.
Bounded ustar imports, document-backed snapshots, grants, CLI commands and the three versioned workspace adapters are now implemented in the harness repository; see its `docs/repository-analysis.md` for limits and local evidence. The model-driven analysis and report-save workflow have now been validated locally after explicit payload authorization, including pending-approval worker interruption and exact RustFS byte comparison. The waves below are candidate groupings, not mandatory prerequisites for that
slice. Reuse existing broker contracts rather than introducing a second SDK or
orchestrator. Native checks require M19; no arbitrary native execution belongs
in a platform WASI component. Risk R alone does not make build scripts safe.

## Purpose

This catalogue defines candidate tools for the first agent harnesses running on the platform. It covers:

- a common tool contract;
- the complete candidate list, grouped by role;
- a delivery order in waves, with the first agents each wave enables;
- shared infrastructure to build before individual tools.

Crate names are **candidates only**. Each must be built for `wasm32-wasip2` and exercised on the platform before it is relied on; native compilation or browser Wasm support is not evidence of WASI compatibility.

## Conventions

### Profiles

The profile states the capabilities a tool component is granted. The broker and the app's network policy enforce it; the label alone grants nothing.

| Profile | Capabilities | Notes |
| --- | --- | --- |
| `ctrl` | None; implemented by the harness or coordinator | Changes run state, not the outside world |
| `pure` | No network, no filesystem preopens | Preferred default. Inputs are bounded bytes or workspace references resolved by the broker |
| `ws` | Workspace service only | Content access is authorized by the workspace service |
| `db` | One approved database endpoint through a scoped service | Never direct credentials to the agent |
| `platform` | Platform control or observability APIs through the broker | Read-only unless marked `X` |
| `egress` | One or more allowlisted external endpoints through the broker | Requires the `wasi:http` egress fix first |
| `svc` | A dedicated bounded service (solver, inference, native sandbox) | Separate deployment and resource controls |
| `native` | Optional Linux sandbox | Out of scope for bootstrapping |

### Risk classes

| Class | Meaning | Default policy |
| --- | --- | --- |
| `R` | Read-only; no state change anywhere | Allowed when in scope |
| `W` | Reversible change inside platform-owned state (workspace revision, memory proposal) | Allowed when in scope; recorded |
| `X` | External, irreversible, or operationally significant effect | Requires approval bound to the exact arguments or artifact digest |

### Naming

- `domain.verb` or `domain.object.verb`, lower case, for example `workspace.read` and `platform.app.status`.
- A tool name and major version identify a fixed schema. Breaking changes publish a new version; running runs keep the pinned version.

## Common tool contract

Every tool, whatever its profile, declares the following in its registry entry. The broker enforces the declaration; the model only sees the name, description, and input schema.

| Field | Content |
| --- | --- |
| `name`, `version` | Stable identifier and semantic version |
| `description` | Model-facing text; no authority implied |
| `input_schema`, `output_schema` | JSON Schema, generated from Rust types |
| `profile`, `risk` | From the tables above |
| `scopes` | Resource scopes required, for example `workspace:{id}:read` |
| `destinations` | Explicit external endpoints, empty for `pure` |
| `limits` | Max input bytes, max output bytes, timeout, max concurrency |
| `cost_class` | Rough cost bucket used by budgets |
| `idempotency` | `natural` (pure/read), `operation_id` (deduplicated by ID), or `none` (needs reconciliation) |
| `cancellation` | `immediate`, `cooperative`, or `not_supported` |
| `retry` | Which error classes are retryable |
| `oversize_strategy` | `truncate_with_marker`, `store_as_artifact`, or `reject` |
| `approval` | `never`, `policy`, or `always` |

Standard request envelope (set by the broker, not by the model):

```text
run_id, lease_generation, operation_id, tenant, caller_identity,
tool_name, tool_version, deadline, input
```

Standard result:

```text
status: ok | denied | invalid_input | limit_exceeded | timeout | cancelled | error | outcome_unknown
output | error { class, message, retryable }
usage { duration_ms, bytes_in, bytes_out, cost_units }
artifacts [ references ]
```

`outcome_unknown` is mandatory for `X` tools whose downstream call may have succeeded before the connection failed. It triggers reconciliation, never a blind retry.

---

## Catalogue

### 1. Control tools

Implemented by the harness and coordinator. Always present; they only change run state.

| Tool | Purpose | Profile | Risk |
| --- | --- | --- | --- |
| `finish(answer, artifacts, action_statuses)` | End the run with a final answer and evidence | ctrl | R |
| `fail(reason)` | End the run explicitly as failed with a user-facing reason | ctrl | R |
| `report_progress(message)` | Emit a bounded progress event to the client | ctrl | R |
| `ask_user(question, options)` | Pause durably for clarification | ctrl | R |
| `request_approval(action, arguments_digest)` | Pause for a decision bound to an exact action | ctrl | R |
| `note(text)` | Scratchpad entry stored in the checkpoint, not shown to the user | ctrl | R |
| `plan.set(steps)` / `plan.update(step, status)` | Maintain a visible task plan | ctrl | R |
| `delegate(agent_type, task, budget, scopes)` | Start a child run with an intersected scope and reserved budget | ctrl | R |
| `await_children(ids, mode)` | Fan-in on all or any child completion | ctrl | R |
| `child.cancel(id)` | Cancel a child run and its outstanding work | ctrl | R |
| `budget.remaining()` | Remaining tokens, calls, time, and child allowance | ctrl | R |
| `time.now()` | Current time supplied by the host, recorded in the checkpoint | ctrl | R |
| `sleep_until(time)` | Durable timer; the run is requeued when due | ctrl | R |
| `wait_for_event(kind, correlation_id, timeout)` | Durable wait on a registered external callback (webhook reply, CI completion); resumes on match or timeout | ctrl | R |
| `child.status(id)` | Progress, budget consumption, and last event of a child run | ctrl | R |
| `child.message(id, text)` | Send bounded guidance to a running child; delivered at its next step | ctrl | R |
| `handoff(agent_type, summary, scopes)` | Transfer the conversation to another agent definition; scopes can only narrow | ctrl | R |
| `run.context()` | Run ID, agent definition, pinned tool set, granted scopes, and caller identity (read-only) | ctrl | R |
| `context.compact(summary, keep_refs)` | Propose replacing older transcript turns with a summary; the coordinator keeps the originals as artifacts | ctrl | R |
| `tools.search(query)` / `tools.describe(name)` | Progressive disclosure of the pinned tool set; never adds tools outside it | ctrl | R |
| `skill.load(name)` | Load a knowledge/skill document pinned by the agent definition into context | ctrl | R |
| `operation.status(operation_id)` | Reconcile an `outcome_unknown` result before deciding to retry or report | ctrl | R |

### 2. Workspace tools

Backed by the S3 workspace service with PostgreSQL manifests.

| Tool | Purpose | Profile | Risk |
| --- | --- | --- | --- |
| `workspace.list(prefix, revision, cursor)` | Paginated listing from the manifest | ws | R |
| `workspace.stat(path, revision)` | Size, digest, content type, revision | ws | R |
| `workspace.read(path, revision, range)` | Read a file or byte range | ws | R |
| `workspace.read_many(paths, revision)` | Batched bounded reads | ws | R |
| `workspace.grep(pattern, prefix, revision)` | Bounded linear-time regex search | ws | R |
| `workspace.find(glob, revision)` | Path matching over the manifest | ws | R |
| `workspace.diff(rev_a, rev_b, path?)` | Changes between revisions | ws | R |
| `workspace.history(path)` | Revisions touching a path | ws | R |
| `workspace.write(path, content, expected_revision)` | Publish a new revision; stale base rejected | ws | W |
| `workspace.apply_patch(patch, base_revision)` | Apply a bounded unified diff conditionally | ws | W |
| `workspace.rename(from, to, expected_revision)` | Manifest-only rename | ws | W |
| `workspace.delete(path, expected_revision)` | Remove a mapping; content retained by policy | ws | W |
| `workspace.branch(base_revision)` | Private branch for a child agent | ws | W |
| `workspace.merge(branch, strategy)` | Three-way merge; conflicts reported, not resolved silently | ws | W |
| `workspace.export(revision, format)` | Produce an archive artifact | ws | R |
| `artifact.publish(revision, path, label)` | Mark a revision as a named deliverable | ws | W |
| `artifact.link(artifact)` | Produce a user-visible reference (never a raw presigned URL in context) | ws | R |
| `workspace.tree(prefix, depth, revision)` | Bounded directory-style view of the manifest | ws | R |
| `workspace.usage()` | Bytes, file count, and quota remaining for the workspace | ws | R |
| `workspace.edit(path, old, new, expected_revision)` | Exact, unique string replacement; fails on zero or multiple matches | ws | W |
| `workspace.write_many(files, expected_revision)` | Atomic multi-file publication as one revision | ws | W |
| `workspace.copy(from, to, expected_revision)` | Manifest-only copy; content deduplicated by digest | ws | W |
| `workspace.revert(paths, to_revision, expected_revision)` | Restore paths to an earlier revision as a new revision | ws | W |
| `workspace.tag(revision, label)` | Name a revision for later reference (review base, approved plan input) | ws | W |
| `workspace.import(archive_ref, prefix)` | Unpack an uploaded archive with entry-count, size, ratio, and path-traversal limits | ws | W |
| `artifact.list(run_id)` | Artifacts produced by the current run or its children | ws | R |

### 3. Text and document processing

All `pure`, risk `R`. Inputs are bytes or workspace references the broker resolves.

| Tool | Purpose | Candidate crates |
| --- | --- | --- |
| `text.chunk(doc, max_tokens, overlap)` | Split for context or indexing, preserving offsets | in-house |
| `text.excerpt(doc, query, max_bytes)` | Bounded excerpt with source position | in-house |
| `text.truncate(doc, max_bytes)` | Truncate with explicit marker | in-house |
| `text.stats(doc)` | Lines, words, tokens, language guess | `whatlang` |
| `text.normalize(doc, form)` | Unicode normalization, whitespace cleanup | `unicode-normalization` |
| `text.diff(a, b)` | Human-readable diff | `similar` |
| `text.template(template, data)` | Render reports from structured data, sandboxed | `minijinja` |
| `markdown.to_text(doc)` | Plain text | `pulldown-cmark` |
| `markdown.outline(doc)` | Heading tree and anchors | `pulldown-cmark` |
| `markdown.links(doc)` | Extract links and references | `pulldown-cmark` |
| `markdown.to_html(doc)` | Rendered HTML for reports | `pulldown-cmark` |
| `html.extract(html)` | Main text, title, links, metadata | `scraper`, `html5ever` |
| `html.to_markdown(html)` | Convert fetched pages to Markdown | `htmd` or in-house |
| `csv.inspect(file)` | Columns, types, row count | `csv` |
| `csv.query(file, filter, columns, aggregate)` | Filter and aggregate | `csv` |
| `json.query(doc, expr)` | jq-style extraction | `jaq` |
| `json.patch(doc, patch)` | RFC 6902 patch | `json-patch` |
| `yaml.to_json(doc)` / `toml.to_json(doc)` | Normalize configuration formats | `serde_yaml`, `toml` |
| `xml.extract(doc, path)` | Simple path extraction | `quick-xml` |
| `regex.extract(text, pattern)` | Structured extraction, linear-time engine, pattern-size limit | `regex` |
| `pdf.extract_text(file, pages)` | Text from PDFs; measure memory | `lopdf`, `pdf-extract` |
| `docx.extract_text(file)` | Text from Word documents | `docx-rs` or zip and XML |
| `tokens.count(text, model_profile)` | Exact count with the pinned tokenizer | tokenizer crate plus pinned vocabulary |
| `citations.verify(quote, source_ref)` | Check a quoted passage exists in the cited revision | in-house |
| `text.decode(bytes, encoding?)` | Detect and convert legacy encodings to UTF-8 | `encoding_rs`, `chardetng` |
| `text.segment(doc, unit)` | Sentence, word, or grapheme segmentation | `unicode-segmentation` |
| `text.similarity(a, b, metric)` | Edit-distance or token-overlap similarity | `strsim` |
| `text.dedupe(passages, threshold)` | Remove near-duplicate passages before they reach context | in-house (MinHash or shingles) |
| `text.redact(doc, rules)` | Replace matched secrets or PII with stable placeholders | `regex` plus `secrets.scan` rules |
| `mime.detect(bytes)` | Content type from magic bytes, never from the file name alone | `infer` |
| `html.sanitize(html, policy)` | Allowlist-based sanitizing before rendering reports | `ammonia` |
| `json.diff(a, b)` | Structural diff producing an RFC 6902 patch | `json-patch` |
| `json.merge_patch(doc, patch)` | RFC 7396 merge patch | `json-patch` |
| `jsonl.query(file, expr, limit)` | Stream newline-delimited JSON with a jq filter | `jaq` |
| `csv.to_json(file, limit)` | Typed rows for downstream tools | `csv` |
| `spreadsheet.read(file, sheet, range)` | Read XLSX, XLS, or ODS cells | `calamine` |
| `log.parse(lines, format)` | Parse JSON, logfmt, or common log formats into records | `serde_json`, in-house logfmt |
| `email.parse(message)` | Headers, body parts, and attachment list from RFC 5322 messages | `mail-parser` |
| `ics.parse(file)` | Events from iCalendar files | `icalendar` |
| `archive.list(file)` / `archive.extract(file, path)` | Inspect or read one entry of a zip or tar archive; ratio and size limits | `zip`, `tar`, `flate2` |
| `sql.format(query, dialect)` | Readable SQL for reports and reviews | `sqlformat` |
| `markdown.format(doc)` | Normalize Markdown formatting | `comrak` |
| `markdown.toc(doc)` | Generate a table of contents with stable anchors | `pulldown-cmark` |

### 4. Deterministic compute

All `pure`, risk `R`. These replace arithmetic and reasoning the model would otherwise approximate.

| Tool | Purpose | Candidate crates |
| --- | --- | --- |
| `math.eval(expr)` | Exact decimal arithmetic | `rust_decimal` plus small parser |
| `math.stats(values)` | Mean, median, percentiles, standard deviation | in-house |
| `units.convert(value, from, to)` | Unit conversion | `uom` or table |
| `currency.convert(amount, from, to, rate_table_ref)` | Conversion against a supplied, dated rate table | `rust_decimal` |
| `time.compute(op, dates, tz)` | Date arithmetic, durations, business days | `jiff` or `chrono` |
| `time.parse(text, tz)` | Parse human or ISO dates | `jiff` |
| `cron.next(expr, from, n)` | Explain and preview schedules | `cron` |
| `hash.digest(data, algo)` | SHA-256 and others for integrity and approval binding | `sha2` |
| `encode.convert(data, from, to)` | Base64, hex, URL encoding | `base64`, `hex` |
| `id.uuid(version)` | UUIDs with host-supplied randomness | `uuid` |
| `graph.analyze(edges, op)` | Cycles, topological order, reachability, shortest path | `petgraph` |
| `semver.compare(a, b)` / `semver.satisfies(version, req)` | Version reasoning | `semver` |
| `ip.classify(address)` / `cidr.contains(cidr, address)` | Network reasoning for policy work | std, `ipnet` |
| `cidr.summarize(ranges)` / `cidr.overlap(a, b)` | Merge ranges and detect overlaps in deny lists or allowlists | `ipnet` |
| `math.regression(x, y)` | Linear fit, slope, and residuals for trend explanations | in-house |
| `math.histogram(values, buckets)` | Distribution summaries | in-house |
| `finance.compute(op, inputs)` | NPV, amortization, compound growth with exact decimals | `rust_decimal` |
| `random.sample(population, n, seed)` | Reproducible sampling; the seed is host-supplied and recorded | `rand_chacha` |
| `duration.parse(text)` | Parse `90s`, `2h30m`, ISO 8601 durations | `humantime`, `jiff` |
| `rrule.expand(rule, from, n)` | Expand iCalendar recurrence rules | `rrule` |
| `geo.distance(a, b)` | Great-circle distance and bounding boxes | `geo` |
| `jwt.decode(token)` | Decode header and claims without trusting them; output marked unverified | `base64`, `serde_json` |
| `checksum.verify(data_ref, digest)` | Verify an artifact against an expected digest | `sha2` |
| `rate.capacity(rps, latency_ms, concurrency)` | Little's-law and quota calculations for rate-limit and concurrency settings | in-house |

### 5. Validation

All `pure`, risk `R`. Let agents check output before claiming success.

| Tool | Purpose | Candidate crates |
| --- | --- | --- |
| `json.validate(doc, schema)` | JSON Schema validation | `jsonschema` with remote references disabled |
| `schema.infer(samples)` | Draft a JSON Schema from examples | in-house |
| `openapi.validate(spec)` | Validate an API description | `openapiv3` |
| `sql.parse(query, dialect)` | Syntax check; classify read versus write | `sqlparser` |
| `rust.parse(source)` | Syntax check of generated Rust | `syn` |
| `rust.format(source)` | Pretty-print generated Rust | `prettyplease` |
| `rust.outline(source)` | Items, signatures, modules | `syn` |
| `toml.validate(doc, schema)` | Configuration validation | `toml` plus `jsonschema` |
| `markdown.lint(doc)` | Broken internal links and anchors, heading structure | `pulldown-cmark` |
| `mermaid.check(diagram)` | Basic diagram syntax sanity | in-house, limited |
| `regex.validate(pattern)` | Check a pattern compiles within limits | `regex` |
| `url.validate(url, policy)` | Parse and check against a domain allowlist | `url` |
| `secrets.scan(text)` | Detect credentials before publication or logging | in-house pattern set |
| `license.check(spdx_expr, policy)` | Evaluate license expressions | `spdx` |
| `platform.manifest.validate(manifest)` | Validate an app manifest with the platform's own rules | reuse `crates/config` validation if it builds for WASI |
| `platform.policy.lint(policy)` | Flag permissive defaults such as empty allowlists | reuse `crates/common` policy types |
| `platform.trigger.validate(triggers)` | Check `[[triggers]]` intervals, routes, concurrency, and timeouts against node limits | reuse `crates/config` validation |
| `platform.placement.check(manifest, namespace)` | Confirm `every_node` placement and same-namespace `local_dependencies` close over every `.internal` URL | in-house plus `crates/common` types |
| `wit.parse(source)` | Parse WIT packages and list worlds, interfaces, and functions | `wit-parser` |
| `wasm.inspect(component)` | Component imports, exports, custom sections, and size | `wasmparser`, `wit-component` |
| `wasm.imports.check(component, allowlist)` | Reject components importing anything outside an allowlist (M18 admission precheck) | `wasmparser` |
| `promql.parse(expr)` | Syntax-check queries before `platform.metrics.query` | `promql-parser` |
| `json.schema.diff(old, new)` | Classify schema changes as compatible or breaking | in-house |
| `openapi.diff(old, new)` | Breaking-change report between API descriptions | `openapiv3` plus in-house |
| `sql.lint(query, rules)` | Flag unbounded scans, missing `WHERE` on writes, and implicit casts | `sqlparser` |
| `migration.check(sql)` | Flag locking or destructive DDL in migrations (column drops, non-concurrent indexes) | `sqlparser` |
| `yaml.lint(doc)` | Duplicate keys, anchors, and implicit typing surprises | `serde_yaml` plus in-house |
| `workflow.lint(file)` | Check CI workflow files for unpinned actions and broad token permissions | `serde_yaml` plus in-house |
| `cron.validate(expr)` | Reject invalid or excessively frequent schedules | `cron` |
| `email.validate(address)` | Syntax check without delivery | `email_address` |
| `pii.scan(text, categories)` | Detect personal data before publication or memory promotion | in-house pattern set |
| `injection.scan(text)` | Flag instruction-like content in retrieved or tool-returned text; advisory, never a security boundary | in-house heuristics |
| `commit.lint(message, convention)` | Check commit or pull-request titles against a convention | in-house |

### 6. Code and repository analysis

`pure` unless noted, risk `R`. Operate on workspace content, never on a host checkout.

| Tool | Purpose | Candidate crates |
| --- | --- | --- |
| `code.parse(source, language)` | Syntax tree and outline for several languages | `tree-sitter` with grammars (C build for WASI; validate) |
| `code.symbols(revision, prefix)` | Symbol index for a workspace subtree | `tree-sitter` or `syn` |
| `cargo.manifest.read(file)` | Parse `Cargo.toml` | `cargo_toml` |
| `cargo.lock.graph(file)` | Dependency graph and duplicate versions | `cargo-lock` |
| `advisory.check(lockfile, db_ref)` | Match dependencies against a pinned RustSec database snapshot | `rustsec` (offline mode; validate) |
| `patch.validate(patch, base_revision)` | Check a diff applies cleanly before publishing | `diffy` |
| `patch.summarize(patch)` | Files, hunks, added and removed lines | `diffy` |
| `changelog.draft(rev_a, rev_b)` | Structured change list from revisions | in-house |
| `repo.read(repo, path, ref)` | Read from an approved remote repository | egress |
| `repo.search(repo, query)` | Code search through the provider API | egress |
| `repo.list_prs(repo, filter)` / `repo.read_pr(repo, id)` | Inspect pull requests | egress |
| `ci.status(repo, ref)` | Read CI results | egress |
| `code.references(revision, symbol)` | Definition and reference locations within a workspace revision | `tree-sitter` or `syn` |
| `code.callers(revision, symbol)` | Syntactic call sites of a function within a workspace revision | `tree-sitter` |
| `context.build(query, revision, token_budget)` | Token-budgeted context packet with paths, line ranges, revision and omitted-context warnings; returned as an artifact | in-house over `code.*` and the index |
| `code.type_definition(revision, position)` | Type-resolved definition through rust-analyzer; runs in the native sandbox | sandbox |
| `code.metrics(source, language)` | Size, nesting depth, and cyclomatic complexity per item | `tree-sitter` |
| `cargo.workspace.describe(revision)` | Members, shared dependencies, features, and targets from manifests | `cargo_toml` |
| `cargo.features(manifest)` | Feature graph and default features | `cargo_toml` |
| `dependency.policy.check(lockfile, policy)` | Banned crates, allowed sources, duplicate versions, and license policy | `cargo-lock`, `spdx` |
| `sbom.generate(lockfile, format)` | CycloneDX or SPDX bill of materials | `cyclonedx-bom` or in-house |
| `toolchain.read(file)` | Pinned toolchain, components, and targets from `rust-toolchain.toml` | `toml` |
| `test.results.parse(file)` | JUnit or libtest JSON results to structured failures | `quick-junit`, `serde_json` |
| `sarif.read(file)` | Static-analysis findings in SARIF format | `serde_json` |
| `commit.draft(patch, convention)` | Draft a commit message from a patch summary; output only | in-house |
| `repo.compare(repo, base, head)` | Commits and changed files between refs | egress |
| `repo.list_issues(repo, filter)` / `repo.read_issue(repo, id)` | Inspect issues | egress |
| `ci.logs(repo, run_id, job, range)` | Bounded, redacted CI log excerpts | egress |

### 7. Platform self-operation

Through the broker to the platform's control and observability APIs. This is how the platform operates with its own agents.

| Tool | Purpose | Profile | Risk |
| --- | --- | --- | --- |
| `platform.apps.list(namespace)` | Deployed apps, versions, placement | platform | R |
| `platform.app.status(app)` | Health, instances, restarts, last traps | platform | R |
| `platform.app.config(app)` | Effective configuration, secrets redacted | platform | R |
| `platform.policy.explain(app)` | Effective network, filesystem, resource policy | platform | R |
| `platform.nodes.list()` / `platform.node.status(node)` | Node health and capacity | platform | R |
| `platform.logs.query(app, since, filter, limit)` | Bounded, redacted log search | platform | R |
| `platform.traces.query(filter)` | Trace lookup through the observability stack | platform | R |
| `platform.metrics.query(expr, range)` | Prometheus-style queries | platform | R |
| `platform.alerts.active()` | Active alerts from Alertmanager | platform | R |
| `platform.audit.query(filter)` | Audit events for the caller's scope | platform | R |
| `platform.billing.usage(namespace, range)` | Resource usage and cost | platform | R |
| `platform.deploy.plan(manifest)` | Dry run showing the changes | platform | R |
| `platform.deploy.apply(plan_digest)` | Apply exactly the approved plan | platform | X |
| `platform.app.scale(app, replicas)` | Change replica count | platform | X |
| `platform.app.restart(app)` | Restart instances | platform | X |
| `platform.app.rollback(app, version)` | Roll back to a prior version | platform | X |
| `platform.route.update(route, plan_digest)` | Change routing | platform | X |
| `platform.config.override(key, value)` | Hot configuration override | platform | X |
| `platform.routes.list(namespace)` | Edge routes and their target apps | platform | R |
| `platform.gateway.show(app)` | Effective auth, CORS, rate-limit, and circuit-breaker settings | platform | R |
| `platform.gateway.state(app)` | Current circuit-breaker state and rate-limit rejections | platform | R |
| `platform.triggers.list(app)` | Declared triggers, last fire time, skipped intervals, failures | platform | R |
| `platform.secrets.list(app)` | Bound secret key names and binding audit; never values | platform | R |
| `platform.deploy.history(app)` | Deployed versions, deploy intents, approvers, and timestamps | platform | R |
| `platform.app.diff(app, version_a, version_b)` | Manifest and policy differences between deployed versions | platform | R |
| `platform.network.flows(app, since)` | Observed connections from the eBPF monitor, with policy verdicts | platform | R |
| `platform.storage.usage(node)` | Disk usage and garbage-collection status | platform | R |
| `platform.release.status()` | Node binary versions and release provenance across the cluster | platform | R |
| `platform.incident.timeline(app, window)` | Correlated deploys, alerts, restarts, and error spikes (composed from the read tools) | platform | R |
| `platform.gateway.update(app, plan_digest)` | Change auth, CORS, rate-limit, or circuit-breaker settings from an approved plan | platform | X |
| `platform.gateway.reset(app)` | Reset a tripped circuit breaker | platform | X |
| `platform.trigger.pause(app, id)` / `platform.trigger.resume(app, id)` | Stop or restart a scheduled trigger | platform | X |
| `platform.node.drain(node)` / `platform.node.cordon(node)` | Move work away from or stop scheduling on a node | platform | X |
| `platform.alerts.silence(matcher, duration, reason)` | Time-bounded Alertmanager silence with a mandatory reason | platform | X |

Agents must never receive platform control-plane or NATS credentials; these tools call the platform through the broker under a scoped service identity.

### 8. Data access

| Tool | Purpose | Profile | Risk |
| --- | --- | --- | --- |
| `db.schema.describe(database)` | Tables, columns, indexes of an approved database | db | R |
| `db.query(database, sql, params)` | Read-only query; `sql.parse` classification plus a read-only role; row and byte limits | db | R |
| `db.explain(database, sql)` | Query plan without execution | db | R |
| `db.execute(database, sql, params)` | Write statement against an approved schema | db | X |
| `kv.get(namespace, key)` / `kv.put(namespace, key, value)` | Small agent-owned key-value state | db | R / W |
| `kv.list(namespace, prefix)` / `kv.delete(namespace, key)` | Enumerate or remove agent-owned keys | db | R / W |
| `kv.compare_and_set(namespace, key, expected, value)` | Conditional update for coordination between runs | db | W |
| `db.sample(database, table, n)` | Bounded random rows with configured columns masked | db | R |
| `db.stats(database, table)` | Row estimates, table and index sizes, last vacuum or analyze | db | R |
| `db.migrations.status(database)` | Applied and pending migrations | db | R |
| `objects.list(bucket_ref, prefix)` / `objects.head(bucket_ref, key)` | Inspect an approved object-storage bucket (M15) | svc | R |
| `objects.get(bucket_ref, key, range)` | Bounded object read through the storage adapter | svc | R |
| `objects.put(bucket_ref, key, content, if_match)` | Conditional write to an approved bucket outside the workspace | svc | X |
| `queue.publish(subject_ref, message)` | Publish to a pre-registered, restricted NATS subject; requires the M6 message-trigger account model | svc | X |

### 9. Retrieval and knowledge

| Tool | Purpose | Profile | Risk |
| --- | --- | --- | --- |
| `documents.search(query, scope)` | Keyword and metadata search over an approved corpus | ws | R |
| `documents.get(id, revision)` | Fetch a document by identifier | ws | R |
| `index.build(scope)` | Build or refresh a search index for a workspace scope | ws | W |
| `embeddings.search(query, scope)` | Semantic search; add only when keyword search falls short | svc | R |
| `rerank(query, candidates)` | Reorder retrieved passages | svc | R |
| `http.fetch(url)` | GET from allowlisted domains; size cap; HTML converted to text | egress | R |
| `web.search(query)` | One search provider behind the broker | egress | R |
| `feed.read(url)` | RSS or Atom from allowlisted feeds | egress | R |
| `memory.search(query, scope)` | Long-term records with provenance, tenant-filtered | ws | R |
| `memory.propose(fact, source, expiry)` | Propose a memory; promotion is a controlled step | ws | W |
| `memory.forget(id)` | Request deletion of a memory and its derivatives | ws | W |
| `memory.list(scope, cursor)` / `memory.get(id)` | Inspect stored memories with provenance and expiry | ws | R |
| `knowledge.search(query)` | Search the knowledge/skill documents pinned by the agent definition | ws | R |
| `platform.docs.search(query)` | Search a pinned snapshot of this repository's `docs/` operator manual | ws | R |
| `http.head(url)` | Status, size, and content type before a full fetch | egress | R |
| `sitemap.read(url)` | Page list from an allowlisted site's sitemap | egress | R |
| `api.call(target_id, operation_id, params)` | Read-only call to a pre-registered OpenAPI operation; the model never supplies the URL | egress | R |
| `package.info(registry, name, version)` | Crate or package metadata, yanked status, and owners | egress | R |
| `advisory.feed(source, since)` | Recent security advisories from a pinned source | egress | R |

### 10. Model-backed tools

Separate model calls with their own budgets, used as tools rather than as the main loop.

| Tool | Purpose | Profile | Risk |
| --- | --- | --- | --- |
| `llm.summarize(doc, length, focus)` | Bounded summary retaining source references | svc | R |
| `llm.extract(doc, schema)` | Structured extraction validated against a schema | svc | R |
| `llm.classify(text, labels)` | Classification with a fixed label set | svc | R |
| `llm.translate(text, target_language)` | Translation | svc | R |
| `llm.critique(artifact, criteria)` | Reviewer-style feedback; advisory only | svc | R |
| `embeddings.create(texts)` | Vectors for indexing | svc | R |
| `llm.judge(output, rubric)` | Rubric-scored evaluation with per-criterion reasons; used by evaluations, never for approval | svc | R |
| `llm.compare(a, b, criteria)` | Pairwise preference with justification | svc | R |
| `llm.rewrite(text, style, constraints)` | Rewrite for audience, tone, or length | svc | R |
| `llm.query_expand(question)` | Generate search queries for retrieval tools | svc | R |
| `vision.describe(image, focus)` | Describe images or screenshots | svc | R |
| `ocr.extract(image_or_pdf, pages)` | Text from scanned pages with positions | svc | R |
| `speech.transcribe(audio)` | Transcript with timestamps | svc | R |
| `moderation.check(text)` | Content-policy classification before publication | svc | R |

### 11. Reasoning services

From the investigation's symbolic-reasoning section. Optional; add when a concrete workload needs them.

| Tool | Purpose | Profile | Risk |
| --- | --- | --- | --- |
| `constraints.check(schema_id, candidate)` | Validate a proposal against fixed application constraints (Z3) | svc | R |
| `constraints.counterexample(schema_id, property)` | Search for a violation of a stated property | svc | R |
| `schedule.solve(tasks, resources, objective)` | Scheduling and allocation (CP-SAT) | svc | R |
| `optimize.linear(problem)` | Linear or mixed-integer optimization (HiGHS) | svc | R |
| `policy.evaluate(policy_id, input)` | Query reviewed rules (OPA); agents cannot change policy | svc | R |
| `rules.eval(expr_id, input)` | Evaluate a reviewed CEL expression in-process | pure | R |
| `datalog.query(rules_id, facts, query)` | Derive facts such as transitive access or dependency closure | pure | R |
| `sat.solve(cnf)` | Small boolean satisfiability problems within a clause limit | pure | R |
| `simulate.monte_carlo(model, n, seed)` | Bounded stochastic estimates with a recorded seed | pure | R |
| `routing.solve(stops, constraints)` | Vehicle-routing or assignment problems (OR-Tools) | svc | R |

Candidate crates for the `pure` entries: `cel-interpreter`, `ascent` or `crepe`, `varisat`, and `rand_chacha`. Validate each on `wasm32-wasip2` before relying on it.

### 12. Media

| Tool | Purpose | Profile | Candidate crates |
| --- | --- | --- | --- |
| `image.inspect(file)` | Dimensions, format, metadata | pure | `image` |
| `image.resize(file, max_dim)` | Thumbnails or downsizing for model input | pure | `image` |
| `image.strip_metadata(file)` | Remove EXIF before publication | pure | `image`, `kamadak-exif` |
| `chart.render(spec)` | Render a chart from structured data to SVG | pure | `plotters` (SVG backend) |
| `diagram.render(mermaid)` | Render diagrams; likely needs a service | svc | — |
| `image.convert(file, format, quality)` | Format conversion with dimension limits | pure | `image` |
| `image.crop(file, rect)` | Crop regions for focused model input | pure | `image` |
| `svg.rasterize(file, width)` | Render SVG to PNG without external resources | pure | `resvg` |
| `qr.encode(text)` / `qr.decode(image)` | Generate or read QR codes | pure | `qrcode`, `rqrr` |
| `pdf.split(file, ranges)` / `pdf.merge(files)` | Page-level PDF manipulation | pure | `lopdf` |
| `pdf.render(markdown_or_html)` | Produce PDF reports; a typesetting engine likely needs a service | svc | — |
| `audio.inspect(file)` | Duration, codec, and channels | pure | `symphonia` |

### 13. External actions

All `egress`, risk `X`, approval `policy` or `always`. Destinations are pre-registered targets, never model-supplied URLs.

| Tool | Purpose |
| --- | --- |
| `tasks.create(project, title, body)` | Create a ticket in an approved tracker |
| `tasks.comment(task_id, body)` | Comment on an existing ticket |
| `tasks.update(task_id, fields)` | Change status or fields |
| `repo.create_branch(repo, base)` | Create a branch in an approved repository |
| `repo.open_pr(repo, branch, patch_revision, body)` | Open a pull request from an exact workspace revision |
| `repo.comment_pr(repo, id, body)` | Review comments |
| `notify.send(channel_id, message)` | Message a pre-registered Slack, Teams, or webhook channel |
| `email.draft(to, subject, body)` | Create a draft without sending |
| `email.send(draft_id)` | Send an approved draft |
| `webhook.post(target_id, payload)` | Call a pre-registered target |
| `calendar.propose(event)` | Propose an event without sending invitations |
| `tasks.close(task_id, resolution)` / `tasks.assign(task_id, assignee)` | Close or reassign a ticket |
| `repo.create_issue(repo, title, body, labels)` | File an issue in an approved repository |
| `repo.label(repo, id, labels)` | Add or remove labels on issues or pull requests |
| `repo.request_review(repo, id, reviewers)` | Request reviewers on a pull request |
| `repo.merge_pr(repo, id, expected_head)` | Merge only if the head still matches the approved commit; approval `always` |
| `release.create(repo, tag, notes_artifact)` | Publish a release from an approved notes artifact; approval `always` |
| `chat.reply(thread_id, message)` | Reply in a pre-registered conversation thread |
| `notify.page(oncall_id, summary, severity)` | Page an on-call rotation; approval `always` and rate-limited |
| `docs.publish(target_id, artifact)` | Publish an approved artifact to a registered documentation site |
| `artifact.share(artifact, recipient_id)` | Grant a registered principal access to an artifact |
| `feature_flag.set(flag_id, value, expiry)` | Change a pre-registered flag with a mandatory expiry |
| `calendar.create(event)` | Send invitations for an approved proposal; approval `always` |

### 14. Native execution (later)

Require the optional Linux sandbox manager from the investigation. Not part of bootstrapping.

| Tool | Purpose | Risk |
| --- | --- | --- |
| `cargo.check(revision, package)` | Compile check of a workspace revision | R |
| `cargo.test(revision, filter)` | Run tests | R |
| `cargo.clippy(revision)` | Lints | R |
| `wasm.build(revision, target)` | Build a component for `wasm32-wasip2` | R |
| `shell.run(command, revision)` | Arbitrary commands in a sandbox | X |
| `browser.open(url)` / `browser.screenshot(url)` | Browser automation | R |
| `package.install(name, version)` | Dependency installation inside the sandbox | X |
| `cargo.fmt_check(revision)` | Formatting check without rewriting | R |
| `cargo.doc(revision, package)` | Build documentation and report broken intra-doc links | R |
| `cargo.deny(revision)` | Advisory, license, ban, and source checks with the repository policy | R |
| `wasm.run(component, input, limits)` | Run a generated `pure` component on the M18 isolated backend; approval before first execution | X |
| `db.ephemeral(migrations_revision)` | Throwaway PostgreSQL to test migrations; destroyed with the sandbox | R |
| `fuzz.run(revision, target, duration)` | Time-bounded fuzzing; crashes stored as artifacts | R |
| `browser.interact(url, steps)` | Click and fill on allowlisted pages; submission actions require approval | X |

### 15. Agent-system operations

Tools for operating, evaluating, and improving the agent system itself. They read coordinator and registry state through the broker; none writes run state directly, consistent with the repository decision above.

| Tool | Purpose | Profile | Risk |
| --- | --- | --- | --- |
| `agent.runs.list(filter)` | Runs by status, agent, tenant, and time window | platform | R |
| `agent.run.inspect(run_id)` | Checkpoints, steps, tool calls, approvals, and costs of a run in the caller's scope | platform | R |
| `agent.run.trace(run_id)` | The run's distributed trace across harness, broker, tools, and gateway (needs M13) | platform | R |
| `agent.run.replay(run_id, harness_revision)` | Replay recorded tool results against a harness revision with the fake provider; no external effects | svc | R |
| `agent.definition.validate(definition)` | Check pinned harness revision, tool versions, scopes, and budgets | pure | R |
| `agent.registry.diff(rev_a, rev_b)` | Tool schema, scope, and limit changes between registry revisions | pure | R |
| `agent.eval.run(suite_id, agent_definition)` | Run a pinned evaluation suite against a definition in an isolated namespace | svc | R |
| `agent.eval.compare(result_a, result_b)` | Score deltas and regressions between evaluation runs | pure | R |
| `agent.usage.report(tenant, range)` | Tokens, tool time, and storage per agent from M14 usage records | platform | R |
| `agent.run.cancel(run_id, reason)` | Request cancellation of a run outside the caller's own tree; operator scope only | platform | X |

---

## Delivery waves

Each wave lists its tools, what it proves, and the first agents it enables.

### Wave 0: shared infrastructure

Build these before individual tools:

- **`agent-tool-sdk` crate.** Shared request envelope, result and error taxonomy, limit enforcement, and a `wasi:http` handler wrapper. Input and output types generate JSON Schemas (for example through `schemars`).
- **Registry generator.** Produces registry entries from SDK declarations, so the broker enforces exactly what each tool declares.
- **Fixture runner.** Replays recorded inputs against each tool natively and as a `wasm32-wasip2` component and compares outputs. Every tool is tested on the real target from day one.
- **Prerequisite platform work.** The `wasi:http` egress fix and the agent network profile from [PLATFORM_HOSTED_HARNESS.md](PLATFORM_HOSTED_HARNESS.md).

### Wave 1: first milestone

| Area | Tools |
| --- | --- |
| Control | `finish`, `fail`, `report_progress`, `ask_user`, `budget.remaining`, `time.now`, `run.context` |
| Workspace | `workspace.list`, `workspace.stat`, `workspace.read`, `workspace.write` |
| Pure | `text.chunk`, `text.excerpt`, `markdown.to_text`, `json.validate`, `math.eval`, `hash.digest`, `mime.detect`, `text.decode` |

Proves: the tool contract, workspace publication, and pure components, all without external network access.

Enables: **document summarizer**. "Summarize the approved documents and save the summary."

### Wave 2: security model

| Area | Tools |
| --- | --- |
| Control | `request_approval`, `plan.set`, `plan.update`, `operation.status` |
| Workspace | `workspace.grep`, `workspace.find`, `workspace.apply_patch`, `workspace.edit`, `workspace.diff` |
| Retrieval | `http.fetch` (allowlisted), `http.head`, `documents.search`, `knowledge.search` |
| Text | `html.to_markdown`, `html.sanitize`, `text.dedupe`, `text.redact` |
| Validation | `secrets.scan`, `pii.scan`, `injection.scan`, `citations.verify`, `url.validate` |
| Action | `tasks.create` |

Proves: egress allowlisting through the broker, approval binding, `outcome_unknown` reconciliation, and redaction before publication.

Enables: **research assistant** (fetch, cite, verify) and **task creator** ("create a task from that summary").

### Wave 3: dogfooding

| Area | Tools |
| --- | --- |
| Platform (read) | `platform.apps.list`, `platform.app.status`, `platform.app.config`, `platform.policy.explain`, `platform.logs.query`, `platform.metrics.query`, `platform.alerts.active`, `platform.audit.query`, `platform.routes.list`, `platform.gateway.show`, `platform.triggers.list`, `platform.deploy.history`, `platform.incident.timeline` |
| Platform (plan) | `platform.deploy.plan` |
| Knowledge | `platform.docs.search` |
| Text | `log.parse` |
| Validation | `platform.manifest.validate`, `platform.policy.lint`, `platform.trigger.validate`, `platform.placement.check`, `promql.parse`, `toml.validate`, `ip.classify`, `cidr.contains`, `cidr.overlap` |

Proves: scoped access to the platform's own APIs, with no control-plane credentials in agents.

Enables:
- **Ops assistant**: explains why an app is unhealthy from status, logs, metrics, and policy.
- **Manifest reviewer**: checks a proposed deployment for permissive policies, invalid triggers, and `.internal` dependencies not covered by `local_dependencies`.
- **Incident summarizer**: builds a timeline of deploys, alerts, and restarts for an app and time window.

### Wave 4: teams and controlled change

| Area | Tools |
| --- | --- |
| Control | `delegate`, `await_children`, `child.cancel`, `child.status`, `child.message` |
| Workspace | `workspace.branch`, `workspace.merge`, `workspace.tag`, `workspace.write_many`, `artifact.publish` |
| Model-backed | `llm.critique`, `llm.extract` |
| Platform (act) | `platform.deploy.apply`, `platform.app.rollback` |
| Code | `rust.parse`, `rust.format`, `patch.validate`, `patch.summarize`, `cargo.lock.graph`, `advisory.check`, `dependency.policy.check`, `toolchain.read`, `commit.draft` |
| Wasm | `wit.parse`, `wasm.inspect`, `wasm.imports.check` |
| Action | `repo.create_branch`, `repo.open_pr`, `repo.request_review` |

Proves: bounded delegation, shared budgets, branch isolation, and approval on exact revisions or plan digests.

Enables:
- **Planner with researcher and reviewer** children.
- **Change agent**: drafts a manifest or patch; a reviewer checks the exact revision; deployment or the pull request happens only after approval.
- **Dependency auditor**: advisory, policy, and duplicate-version reports from `Cargo.lock`.
- **Component reviewer**: reports a Wasm component's imports, exports, and WIT worlds against its intended network profile.

### Wave 5 and later: by demand

- Data access (`db.*`) once a concrete database use case exists.
- Semantic retrieval (`embeddings.*`, `rerank`) once keyword search measurably falls short.
- Reasoning services (`constraints.*`, `schedule.solve`) for a specific allocation or validation problem.
- Media tools for document-heavy workloads.
- Native execution (`cargo.*`, `shell.run`, `browser.*`) after the Linux sandbox manager exists.
- Object storage and messaging (`objects.*`, `queue.publish`) after M15 and the M6 NATS trigger account model.
- Platform mutations beyond deploy and rollback (`platform.gateway.update`, `platform.trigger.pause`, `platform.node.drain`, `platform.alerts.silence`) once M11 and M12 cover them with scoped, digest-bound identities.
- Agent-system operations (`agent.*`) once the coordinator exposes run inspection and replay; `agent.run.trace` needs M13 and `agent.usage.report` needs M14.
- Generated-component execution (`wasm.run`) only after M17 and M18.

## First agent definitions

Each agent is one harness deployment with a pinned tool set. Scopes are intersected with the caller's permissions at run admission.

| Agent | Wave | Tools | Approval-gated |
| --- | --- | --- | --- |
| `doc-summarizer` | 1 | Control, workspace read/write, text, `json.validate` | None |
| `researcher` | 2 | `http.fetch`, `documents.search`, `citations.verify`, workspace write | None |
| `task-creator` | 2 | Workspace read, `tasks.create`, `secrets.scan` | `tasks.create` |
| `ops-assistant` | 3 | Platform read tools, `platform.policy.lint`, `math.stats` | None (read-only) |
| `manifest-reviewer` | 3 | `platform.manifest.validate`, `platform.policy.lint`, `platform.deploy.plan` | None (read-only) |
| `planner` | 4 | Control including `delegate`, `plan.*`, workspace read | Child budgets |
| `reviewer` | 4 | Workspace read and diff, `llm.critique`, validators | None; cannot publish |
| `change-agent` | 4 | Workspace branch/patch, validators, `platform.deploy.plan`, `platform.deploy.apply`, `repo.open_pr` | `platform.deploy.apply`, `repo.open_pr` |
| `dependency-auditor` | 4 | `cargo.lock.graph`, `advisory.check`, `license.check`, `dependency.policy.check`, `sbom.generate`, `tasks.create` | `tasks.create` |
| `incident-summarizer` | 3 | Platform read tools, `platform.incident.timeline`, `log.parse`, `math.stats`, workspace write | None (read-only on the platform) |
| `component-reviewer` | 4 | `wit.parse`, `wasm.inspect`, `wasm.imports.check`, `platform.policy.explain`, workspace read | None (read-only) |
| `docs-maintainer` | 4 | Workspace branch and edit, `markdown.lint`, `markdown.toc`, `mermaid.check`, `platform.docs.search`, `repo.open_pr` | `repo.open_pr` |
| `release-notes` | 4 | `repo.compare`, `changelog.draft`, `patch.summarize`, `llm.rewrite`, workspace write | `release.create` if enabled |
| `eval-runner` | 5 | `agent.eval.run`, `agent.eval.compare`, `agent.run.replay`, `llm.judge` | None; no external effects |

## Acceptance criteria per tool

Before a tool is registered for agent use:

- Built for `wasm32-wasip2` and exercised on the platform, not only natively.
- Fixture tests pass identically for native and Wasm builds.
- Input, output, and time limits are enforced and tested with oversized and adversarial inputs (deeply nested JSON, pathological regexes, compressed bombs, huge PDFs).
- `pure` tools are deployed with no network and no preopens; a test confirms outbound attempts fail.
- `X` tools return `outcome_unknown` correctly when a connection fails after dispatch, and reconciliation by `operation_id` is tested.
- Error messages and outputs never include credentials, presigned URLs, or unredacted secrets.
- The registry entry matches the implementation; the broker rejects calls outside the declared scopes.
- Archive, image, PDF, and spreadsheet tools enforce entry-count, decompressed-size, dimension, and ratio limits; archive entries with path traversal are rejected.
- Tools that use randomness or time (`random.sample`, `simulate.monte_carlo`, `time.now`) take host-supplied values recorded in the checkpoint, so replay is deterministic.
- Advisory classifiers (`injection.scan`, `pii.scan`, `moderation.check`, `llm.judge`) are documented as advisory; no approval, scope, or egress decision depends on them alone.
- Tools acting on a reviewed object (`repo.merge_pr`, `platform.gateway.update`, `release.create`) fail closed when the current head, digest, or artifact no longer matches the approved one.

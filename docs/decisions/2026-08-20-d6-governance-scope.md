# ADR — D6 governance: scope for v0.7.1

- **Date:** 2026-08-20
- **Status:** Accepted
- **Decision owner:** Tin Dang (selected full-D6 scope for v0.7.1)

## Why this ADR exists at all

D6 was carried through the entire GA plan as a one-line decision —
*"governance metadata + API in the break release, UI post-GA, no roles"* —
derived from the TencentDB Agent Memory competitive analysis. It slipped from
0.7.0 to 0.7.1 without ever being written down.

Searching for a specification found **nothing**: no document under `docs/`, and
nothing in `.planning/` beyond a single blueprint risk row (`blueprint.md:605`):

> Users misuse `forget(scope)` and nuke data → Soft-delete by default;
> `.hard()` flag required for real delete; **audit log**

So "ship D6" first means "decide what D6 is". This ADR does that, and it is
written **before** any code because the audit below changed what the work is.

## What already exists (audited, not assumed)

D6 does not start from zero. `crates/lunaris-core/src/audit.rs` already defines
a canonical `AuditEvent` (externally tagged on `kind` for grep-friendly ops
triage) with five variants, and they are **genuinely wired** — producers live in
`lunaris/src/handle.rs` (forget), `lunaris-verify` (arbitration),
`lunaris-consolidate` (promotion / archive), and the reflect path
(`ReflectInvalidation`).

Events publish to a fixed topic:

```rust
pub const AUDIT_TOPIC: &str = "__lunaris_audit__";
```

That is a real foundation. It is also, as it stands, **not an audit trail**.
Three defects, each verified in source:

### G1 — every tenant's audit events land in ONE scope — **CLOSED (W4.6, 0.7.0)**

`impl Publisher for Arc<dyn StoragePort>` published under `Scope::dev()`:

```rust
// RFC 0001 Wave 0: audit publishes use Scope::dev() as migration crutch.
StoragePort::publish(self.as_ref(), &Scope::dev(), topic, partition, payload)
```

It was honestly labelled (`scope-dev-allowed: audit-publish-trait-surface`,
tracked as Wave 1E), and as *retrieval* debt it was tolerable. As the substrate
for a **governance** feature it was disqualifying: a governance read API over a
dev-scoped stream serves one tenant another tenant's history. CLAUDE.md already
names `Scope::dev()` a migration crutch and says any new production call site is
a carry-over, not a steady state.

`Publisher::publish` now takes `&Scope` and all six production
`publish_audit_event` call sites forward the scope they already hold.
`crates/lunaris/tests/audit_scope_isolation.rs` drives
`ScopedLunaris::forget` under a real scope and reads the receipt back off that
scope's own audit topic.

One correction to the diagnosis above, found while fixing it: the marker said
the debt was "tracked in docs/v0.3-known-debt.md as Wave 1E". That file has a
Wave 1E note, but for a different item — `Lunaris::recall`'s queue-depth health
check. The audit-publish debt had no entry anywhere. A marker that names a
tracking location is only as good as the entry it names.

### G2 — nothing reads the events back — **CLOSED (W4.6, 0.7.0)**

`grep` for a consumer against `AUDIT_TOPIC` returns **producers only**. Nothing
in this repo consumes what is published. A write-only audit log answers no
governance question — "who deleted this?" is exactly the query the feature
exists to serve.

The gap is a missing **caller**, not missing infrastructure, and that materially
lowers the cost of D6.3. `StoragePort::subscribe` exists
(`lunaris-core/src/storage/port.rs:250`) and Moon implements it
(`lunaris-storage-moon/src/lib.rs:388`, backed by `queue.rs:107`). The one place
that came close says so outright — `eval_05_helios_10k.rs` keeps its
`AUDIT_TOPIC` / `AuditEvent` imports under `#[allow(unused_imports)]` purely as
documentation:

> The `AuditEvent` + `AUDIT_TOPIC` imports are retained below as documentation
> of the Plan 13-01 wiring consumed by this harness; partition-archaeology on
> `StoragePort::subscribe` would add 80+ LOC for equivalent evidence, which
> Plan 13-02 explicitly authorizes us to skip.

So the consumer was scoped out once, deliberately and with a reason. D6 is where
that deferral comes due.

(Same shape as the curation finding: the RAPTOR/community tree is also only ever
written into. Building a writer and deferring the reader is a recurring failure
mode here, and worth naming as one.)

### G3 — audit loss is silent by contract — **CLOSED (W4.6, 0.7.0)**

`publish_audit_event` is fire-and-forget per blueprint §11: serialize failure
and publish failure both `tracing::warn!` and return `Ok(0)`.

Fire-and-forget is defensible for telemetry. It is not defensible for an audit
trail, where "we have no record" and "it did not happen" must not be the same
observable state. This does not mean audit failures should abort user
operations — it means a dropped audit event must be **countable**, so an
operator can see the gap rather than infer it.

## Decision

**D6 v0.7.1 = provenance + a real audit trail + retention + a read path**,
built in that order, with G1 as a hard prerequisite.

### D6.1 — thread the real scope through the audit path (prerequisite)

Extend `Publisher::publish` to take `&Scope` and forward the caller's real scope
from its `Lunaris`/`ScopedLunaris` handle. This is the Wave 1E work already
described in `docs/v0.3-known-debt.md`; D6 is what makes it mandatory rather
than aspirational.

Exit criterion: **zero** `Scope::dev()` call sites remain on the audit path, and
a test proves two scopes' audit events do not intermingle.

### D6.2 — provenance on every memory

**Extend, do not duplicate.** Episodes already carry `source`, and
`source_priority` already reads it (`distilled:` 95 > `decision:` 90 >
`edit:` 85 > `tool_call` 75). Provenance adds the writer's identity, which
`source` does not carry:

- **writing surface** — `mcp` | `hook` | `cli` | `http`
- **agent / principal identity** as supplied by that surface
- **scope** and **timestamp** (already present; formalised here)

`lunaris-cli` (PR #148) is the reason this is now cheap to verify: a fourth
surface that can be driven from a shell makes "did provenance record the right
writer?" a shell-testable assertion rather than an MCP-session ritual.

### D6.3 — audit trail that can be read

- Publish per-scope (D6.1), not to a single shared partition 0.
- Add the **consumer**: a query path over `__lunaris_audit__` filtered by scope
  and time range.
- Add a **dropped-event counter** so G3 becomes visible. Fire-and-forget stays —
  an audit publish failure must not fail a user's `forget` — but the drop is
  counted and exported, so a silent gap becomes a metric.

**INGEST-04 constraint:** audit writes on the ingest path MUST extend the single
`WriteOp` vector. A second `atomic_write` in `lunaris-ingest/src/pipeline.rs`
breaks the one-atomic-write-per-ingest invariant and its CI grep guard.

### D6.4 — retention

Per-scope retention policy with enforcement. Two known interactions, recorded so
they are not rediscovered mid-build:

- **Soft-delete semantics.** `forget` soft-deletes by default; retention that
  hard-deletes must not silently change what `.hard()` means.
- **The `matched` over-count on soft-deleted records** is a known open
  follow-up. Retention enforcement will surface it, so fix or explicitly scope
  it out at that point.

### D6.5 — read API on the shared dispatch

The governance read path is exposed as `MemoryRequest` variants going through
`lunaris_memory_service::protocol::dispatch`, like everything else — reachable
identically from MCP, hook, CLI, and HTTP.

Rationale is GA-1's, restated: before PR #126 there were three divergent recall
pipelines because each surface planned its own. A governance API that bypasses
`dispatch` becomes the fifth divergent surface, and the one where divergence is
least acceptable — differing answers to "who deleted this?" per surface is worse
than no answer.

## Explicitly out of scope

- **UI / dashboard.** Post-GA per the original decision. The Memory Inspector
  milestone is the right home; it is read-only by design and can consume D6.5.
- **RBAC / roles.** "No roles" was explicit. Auth today is documented-plaintext
  v0; layering roles on it would imply a guarantee the transport does not make.
- **Backfilling audit history.** Events published before D6.1 are `Scope::dev()`
  and cannot be attributed to a tenant after the fact. Attributing them by guess
  would be worse than a documented gap: it would put fabricated provenance into
  the record the feature exists to make trustworthy.

## Consequences

- **D6.1 is a breaking change to `Publisher`** — a public trait in
  `lunaris-core`. It lands in 0.7.1 with the signature change called out in the
  CHANGELOG.
- **Sequencing is forced, not preferred.** D6.1 → D6.3 → D6.4/D6.5. Building the
  read API first would ship a governance surface that reads a cross-tenant
  stream.
- **The audit trail becomes a durability concern.** Backup/restore was drilled
  at RPO=0 / RTO<1s for the main store; audit events published to a Moon topic
  inherit that, and the drill should assert them explicitly rather than by
  assumption.

## Verification

Per `feedback_built_not_wired`, each item needs a test proving the **production
path** exercises it, not merely that the capability compiles:

| item | discriminating test |
|---|---|
| D6.1 | two scopes publish audit events; neither reads the other's |
| D6.2 | an ingest through each of the four surfaces records that surface |
| D6.3 | a `forget` is readable through the query path afterwards |
| D6.3 | a forced publish failure increments the drop counter |
| D6.4 | retention expires a record and leaves soft-delete semantics intact |
| D6.5 | all four surfaces return byte-identical governance answers |

The last row is the one `lunaris-cli` makes cheap, and it is the row that keeps
D6 from becoming the fifth divergent surface.

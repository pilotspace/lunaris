# SPK-CONSOLIDATE-MCP — On-Demand MCP Consolidate Tool

**Status:** SHIPPED (2026-06-09) — P-C `memory.scratchpad_consolidate` merged
via the collapsed design (reuses `WorkingMemory::consolidate()` + 3 guards; see
Section 2). The original Moon-direct design is INVALIDATED (see Section 1). See
CHANGELOG (2026-06-09).
**Quick task:** 260609-dvi (P-C guarded consolidate MVP — collapsed design)
**Owns the design for:** milestone phase P-C — Guarded consolidate MVP
**Date:** 2026-06-09 (original: 2026-06-08)

---

## Section 1 — ORIGINAL DESIGN: INVALIDATED

The original P-C design (recorded 2026-06-08 under quick task 260608-qqb) called for
**bypassing `StoragePort::subscribe` and driving Moon's message queue directly** with a
dedicated consumer group, `pop` → consolidate → `ack` semantics, and a DLQ.

This design is **INVALIDATED** by two primary-source findings in `vendor/moon/`:

### Kill evidence 1 — One hardcoded consumer group (`__mq_consumers`)

`vendor/moon/src/mq/mod.rs:51`:

```rust
/// Reserved consumer group name.
pub const DEFAULT_CONSUMER_GROUP: &str = "__mq_consumers";
```

Moon has **one** hardcoded consumer group for all MQ operations. This is confirmed
by every write path that handles consumer-group semantics:

- `vendor/moon/src/handler_monoio/write.rs:283,296,421,640,653` — all use `__mq_consumers`
- `vendor/moon/src/handler_sharded/write.rs:280,293,421,422,549` — same constant

There is no multi-group PEL (Pending Entry List). A "dedicated MCP consumer group"
distinct from the background worker's group is **impossible** — any group name a
caller supplies maps to the same single `__mq_consumers` group. The "dedicated group"
requirement in the original checklist cannot be satisfied by Moon.

### Kill evidence 2 — `MQ.POP` dot-command does not exist

`vendor/moon/src/mq/mod.rs:59` routes MQ subcommands via `is_mq_command(b"MQ")`.
The dispatch table handles `MQ POP` and `MQ ACK` (space-separated form) — these are
exactly what `lunaris-storage-moon::subscribe` already calls internally (pop-then-inline-ack).

The `moondb` Rust SDK's `MqClient::pop_partitioned` / `push_partitioned` methods would
need to emit `MQ.POP` and `MQ.PUSH` (dot-form) as distinct command codes. These do **not**
appear in Moon's dispatch (`mod.rs:59` lists no dot-form handler). Calling them against
a live Moon server would return an unknown-command error at runtime.

### Required-before-exposure checklist (original — status updated)

- [x] ~~**Dedicated MCP consumer group.**~~ **IMPOSSIBLE** — Moon has one hardcoded `__mq_consumers`
  group; a dedicated MCP group cannot be created.
- [x] ~~**Ack only after successful consolidation.**~~ **RESOLVED differently** — the existing
  `subscribe` path (pop-then-inline-ack) is already ack-safe. The "no ack" concern applied
  to the Postgres path. Moon's pop is synchronous with inline ack.
- [x] **`capabilities().queue_native` gate.** Retained in the collapsed design (Guard 1).
- [x] **Hard drain timeout.** Retained in the collapsed design (Guard 3).

---

## Section 2 — COLLAPSED DESIGN (REPLACES ORIGINAL)

The four-component collapsed design reuses the existing `WorkingMemory::consolidate()`
drain with three bug-fixes and three runtime guards, rather than a Moon-direct rewrite.

### Component 1 — Reuse `WorkingMemory::consolidate()` drain (no MQ-direct rewrite)

`WorkingMemory::consolidate()` already calls `drain_consolidate_events()` which uses
`storage.subscribe()` → Moon `MQ POP` / `MQ ACK` (space form), which is exactly the
only working path on Moon (see kill evidence 2). No Moon-direct rewrite is needed or
possible. No dedicated consumer group, no custom DLQ — the existing pop-then-inline-ack
path is the correct and only available path.

### Component 2 — T1 fixes (three correctness bugs)

**(a) Scope-dev mismatch (drain_consolidate_events used Scope::dev())**

`crates/lunaris/src/primitives/working_memory.rs` — `drain_consolidate_events()` was
subscribing under `Scope::dev()` regardless of the calling `WorkingMemory`'s real scope.
Events published under the server's real scope were never consumed. Fixed in 260609-dvi T1:
`drain_consolidate_events` now takes `scope: &Scope` and subscribes under the real scope.

**(b) Archive-audit parity (promotion-only loop)**

`working_memory.rs:237-244` contained a promotion-only loop that emitted
`AuditEvent::ConsolidatorPromotion` for each promotion but never emitted
`AuditEvent::ConsolidatorArchive` for archives. Fixed: the loop is replaced by a single
call to `lunaris_consolidate::publish_per_event_audits(&storage, &report)` which emits
both promotion and archive audit events — matching the background worker's behavior.

**(c) ActRConsolidator not re-exported from `lunaris` crate**

`lunaris-mcp` must not depend directly on `lunaris-consolidate`. Fixed:
`lunaris::ActRConsolidator` is now re-exported from `crates/lunaris/src/lib.rs`.
`publish_per_event_audits` and `archive_event` are now `pub` in `worker.rs` and
re-exported from `lunaris-consolidate/src/lib.rs`.

### Component 3 — T2: bootstrap ActR install in `lunaris-mcp`

`crates/lunaris-mcp/src/state.rs` — `bootstrap_inner` now calls
`lunaris.consolidator_pipeline().set_consolidator(Arc::new(ActRConsolidator::default()))`
immediately after `Lunaris::open`. This installs the ActR consolidator without spawning
the background worker (`set_consolidator` does NOT call `enable()`). The pipeline stays
`is_enabled() == false`.

Isolation seam added: `bootstrap_inner` gains a fourth param `data_dir_override: Option<&str>`
(default `None` = `"./.lunaris-moon"`) so embedded-moon tests can use unique `tempfile::tempdir()`
paths without cross-process AOF contention.

### Component 4 — T3 guards in `memory.scratchpad_consolidate`

The tool handler applies three circuit-breakers in order:

1. **`queue_native` gate** — `storage.capabilities().queue_native == false` → returns typed
   `UnsupportedBackend` result with an actionable message. Fails fast on `memory://` and
   `sqlite://` backends. No drain is attempted.

2. **`is_enabled` guard** — `consolidator_pipeline().is_enabled() == true` → returns typed
   `WorkerConflict` result. Prevents double-consume on the single `__mq_consumers` group
   when the background worker is live.

3. **Hard timeout** — `tokio::time::timeout(5s, wm.consolidate())` bounds the ~51 s
   worst-case (`DRAIN_CAP=1024 × PULL_TIMEOUT_MS=50ms`). Returns typed `Timeout` result
   on expiry. Injectable via `handle_inner(state, params, dur)` for test isolation
   (tests use `Duration::from_millis(1)`).

DTO discipline:
- `ScratchpadConsolidateParams` has `#[serde(deny_unknown_fields)]` (CLAUDE.md §HTTP DTO).
- No `scope` or `tenant` field on the wire — scope is server-bound (from `AppState::scope`).

---

## Related files (updated)

| File | Relevance |
|------|-----------|
| `vendor/moon/src/mq/mod.rs:51,59` | `__mq_consumers` hardcoded; `MQ.POP` not in dispatch — kill evidence |
| `crates/lunaris/src/primitives/working_memory.rs` | `consolidate()` + `drain_consolidate_events(scope)`; T1 fixes |
| `crates/lunaris-consolidate/src/worker.rs` | `publish_per_event_audits` (now `pub`); `archive_event` (now `pub`) |
| `crates/lunaris-consolidate/src/lib.rs` | `pub use worker::{publish_per_event_audits, archive_event}` added |
| `crates/lunaris/src/lib.rs` | `pub use lunaris_consolidate::ActRConsolidator` added |
| `crates/lunaris-mcp/src/state.rs` | `set_consolidator` added to `bootstrap_inner`; `data_dir_override` param |
| `crates/lunaris-mcp/src/tools/scratchpad_consolidate.rs` | Tool handler with 3 guards + injectable timeout |
| `crates/lunaris/src/ingest.rs:234` | `publish_consolidate_event` gated on `queue_native` (unchanged) |
| `crates/lunaris/src/consolidator_pipeline.rs` | `set_consolidator` (no spawn) / `is_enabled` / `enable` |

# SPK-CONSOLIDATE-MCP — On-Demand MCP Consolidate Tool

**Status:** DEFERRED (analysis complete; implementation gated on the four fixes below)
**Quick task:** 260608-qqb (scratchpad tools)
**Owns the design for:** milestone phase P-C — Guarded consolidate MVP
**Date:** 2026-06-08

## Summary

The scratchpad trio (`memory.scratchpad_write` / `_read` / `_grep`) shipped without
a fourth `memory.scratchpad_consolidate` tool. `WorkingMemory::consolidate()`
**exists** (`crates/lunaris/src/primitives/working_memory.rs`) but must not be
exposed on the MCP surface as-is: doing so today either errors on the default
backend, blocks the stdio transport for ~51 s, or silently competes with — and
loses messages to — the background consolidation worker.

This spike records the four independent blocking issues with file:line evidence
and specifies the path to a *guarded* MVP (the resolved P-C approach: **drive the
drain through the Moon MQ directly with a dedicated consumer group + ack-after-
success**, rather than through the current `StoragePort::subscribe` abstraction,
which has no ack).

---

## Blocking issue 1 — embedded/sqlite `subscribe` returns `NotSupported`

`crates/lunaris-storage-embedded/src/lib.rs:456` — `async fn subscribe(...)`
returns `StorageError::NotSupported`.

`memory.scratchpad_consolidate` would call `WorkingMemory::consolidate()`
→ `drain_consolidate_events()`
(`crates/lunaris/src/primitives/working_memory.rs`)
→ `storage.subscribe(...)`. On the embedded/sqlite backend — the `lunaris-mcp`
default (`memory://`, `sqlite://`) — that call returns `NotSupported`, so the
tool would error on its own default backend. A public tool that fails on the
default configuration is worse than no tool.

## Blocking issue 2 — sqlite never *publishes* consolidate events

`crates/lunaris/src/ingest.rs:234` — `publish_consolidate_event` (defined at
`ingest.rs:227`, called at `ingest.rs:172`) early-returns when
`!storage.capabilities().queue_native`.

The embedded backend does not advertise `queue_native`, so ingest never enqueues
a consolidate event there. Even if `subscribe` were implemented for sqlite, the
queue would be empty — there is nothing to drain. Consolidation is a
**queue-native-only** capability (Moon, Postgres), and the tool must say so
explicitly rather than appearing to "succeed" with an empty report.

## Blocking issue 3 — worst-case ~51 s synchronous blocking

`crates/lunaris/src/primitives/working_memory.rs:42` — `DRAIN_CAP = 1024`;
`working_memory.rs:49` — `PULL_TIMEOUT_MS = 50`; the doc at lines 45-48 states the
drain terminates within `DRAIN_CAP × PULL_TIMEOUT_MS` ms — **worst case ≈ 51.2 s**
when the broker keeps delivering at the timeout boundary.

A stdio MCP tool call that can block for ~51 s will routinely exceed the client's
per-tool timeout (Claude Code / Codex), wedging the single stdio request loop.
The MCP-facing drain needs a **hard wall-clock timeout** far below the client
budget (target: a few seconds), independent of `DRAIN_CAP`.

## Blocking issue 4 — Postgres `subscribe` ignores scope/group; queue is global-per-topic, un-acked

`crates/lunaris-storage-postgres/src/queue.rs:140` — `subscribe(_scope, _group, ...)`
ignores both `_scope` (param at `queue.rs:142`) and `_group` (`queue.rs:143`),
reading purely by topic via `pgmq.read($1, 30::int, 1)` (`queue.rs:151`) — a 30 s
visibility timeout, fetch-1, **with no `ack`/`archive` after the work succeeds**.

Consequences:

- **Global-per-topic, cross-scope consumption.** Events from any scope/namespace
  are visible to any subscriber. `consolidate_scoped(Some(prefix))` filters to the
  namespace *after* consuming, so out-of-scope events are consumed-and-dropped.
- **Silent loss + 30 s redelivery.** Because there is no ack tied to successful
  consolidation, a consumed-but-filtered (or crashed-mid-consolidate) event simply
  reappears after the pgmq visibility timeout — there is no commit point that means
  "this event was consolidated."
- **Background-worker contention.** `enable()`
  (`crates/lunaris/src/consolidator_pipeline.rs:173`) spawns
  `run_consolidate_worker` (`consolidator_pipeline.rs:146`,
  `crates/lunaris-consolidate/src/worker.rs`). That worker subscribes with
  `Scope::dev()` on the **shared** consumer group
  `CONSOLIDATE_CONSUMER_GROUP = "lunaris-consolidate-v0"`
  (`worker.rs:60`, topic `CONSOLIDATE_TOPIC = "__lunaris_consolidate__"`,
  `worker.rs:63`). `WorkingMemory::drain_consolidate_events` *also* uses
  `Scope::dev()` + that same shared group
  (`working_memory.rs:190`). So an MCP-triggered drain and the background worker
  race on the same cursor — either may consume events the other expected.

---

## Required before exposure (the guard)

A `memory.scratchpad_consolidate` tool may ship once **all four** hold:

- [ ] **Dedicated MCP consumer group.** The MCP drain must use its own group/cursor,
  distinct from `lunaris-consolidate-v0`, so it never competes with the background
  worker. (Today `subscribe` ignores `_group` on Postgres and both call sites
  hardcode the shared group.)
- [ ] **Ack only after successful consolidation.** Messages must be acknowledged
  *after* `consolidate_scoped` succeeds, never on mere receipt — so a crash mid-
  consolidate redelivers rather than silently losing the event. `StoragePort` has
  no ack method; this must come from a lower layer (see "Path forward").
- [ ] **`capabilities().queue_native` gate.** The handler must return a typed,
  actionable error on non-queue-native backends (sqlite, `memory://`) instead of
  surfacing `NotSupported` or an empty report. Mirror the gate at
  `crates/lunaris/src/ingest.rs:234`.
- [ ] **Hard drain timeout.** A configurable wall-clock cap (default a few seconds,
  ≪ the ~51 s `DRAIN_CAP × PULL_TIMEOUT_MS` ceiling and ≪ the MCP client timeout)
  so a slow broker cannot wedge the stdio transport.

---

## Path forward (resolved P-C approach) — Moon-direct drain

`StoragePort::subscribe` cannot satisfy "ack only after success" — the trait has
no ack. The resolved decision for P-C is to **bypass the StoragePort abstraction
for consolidate and drive the Moon message queue directly** via the `moondb` MQ
API (`pop` / `ack` / consumer-group / DLQ semantics):

- A **dedicated MCP consumer group** (not `lunaris-consolidate-v0`) so the MCP drain
  has an independent cursor.
- **`pop` → consolidate → `ack`** — ack strictly after `consolidate_scoped`
  succeeds; failures leave the message for redelivery / DLQ.
- A **DLQ** for poison messages and a **hard timeout** bounding the whole drain.
- A **`queue_native` gate** that fails fast (typed error) on sqlite / `memory://`.

Moon-only by construction. Postgres consolidate stays on the background-worker
path until its queue layer grows an ack-capable, group-aware `subscribe`; the MCP
tool will report it as unsupported there rather than silently dropping events.

---

## Related files

| File | Relevance |
|------|-----------|
| `crates/lunaris/src/primitives/working_memory.rs` | `consolidate()` + `drain_consolidate_events()`; `DRAIN_CAP`/`PULL_TIMEOUT_MS`; `Scope::dev()` + shared group |
| `crates/lunaris-storage-embedded/src/lib.rs:456` | `subscribe` → `NotSupported` (default backend) |
| `crates/lunaris/src/ingest.rs:227,234` | `publish_consolidate_event` gated on `queue_native` |
| `crates/lunaris-storage-postgres/src/queue.rs:140` | `subscribe` ignores scope/group; `pgmq.read` 30 s visibility; no ack |
| `crates/lunaris/src/consolidator_pipeline.rs:146,173` | `enable()` spawns `run_consolidate_worker` |
| `crates/lunaris-consolidate/src/worker.rs:60,63` | shared `CONSOLIDATE_CONSUMER_GROUP` / `CONSOLIDATE_TOPIC` |

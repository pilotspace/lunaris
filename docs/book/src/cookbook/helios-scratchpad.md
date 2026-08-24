# Helios Scratchpad

**Reach for `HeliosScratchpad` when an agent needs a filesystem-shaped
working store — `write` / `read` / `edit` / `grep` / `ls` over a
session-scoped namespace — backed by Lunaris's bi-temporal MVCC store, with
`as_of` time-travel for free.**

`HeliosScratchpad` is exported by the umbrella `lunaris` crate (not
`lunaris-recipes`) — `use lunaris::{HeliosScratchpad, Lunaris, Hlc};`. It
was built for [Helios](https://github.com/pilotspace/lunaris), Lunaris's
first downstream consumer, which replaces deepagents' ephemeral `dict`-backed
mock filesystem with a real bi-temporal store. It is a convenience over the
[`WorkingMemory`](./index.md#workingmemory--scope-prefixed-scratchpad)
primitive — Lunaris doesn't know Helios exists; the recipe is not a coupling.

> This chapter is the public-facing recipe summary. For the full
> integration story — multi-session servers, GDPR purge, graph-aware entity
> recall, degraded-state handling, dual-backend portability, and a
> production checklist — see [`docs/helios-integration.md`](https://github.com/pilotspace/lunaris/blob/main/docs/helios-integration.md).

## The frozen 9-method surface

`HeliosScratchpad` holds an `Arc<Lunaris>` + a `session_prefix` (e.g.
`"helios:fs/session-42/"`) + a delegated `WorkingMemory` (itself
`Arc<Lunaris>` + `String`), and is `Clone` (every field is cheap). Its
public surface is frozen at nine symbols — the
`helios_scratchpad_public_surface_under_50_loc` test asserts *exactly* nine
at compile time, so the surface can neither grow nor shrink without an
`HELIOS-*` requirement update:

| # | Method | Signature |
|---|---|---|
| 1 | `new` | `fn new(lunaris: Arc<Lunaris>, scope: Scope, session_id: &str) -> Self` |
| 2 | `write` | `async fn write(path: &str, content: impl Into<String>) -> Result<Lsn, LunarisError>` |
| 3 | `read` | `async fn read(path: &str) -> Result<Option<String>, LunarisError>` |
| 4 | `edit` | `async fn edit(path: &str, _old: &str, new: &str) -> Result<Lsn, LunarisError>` |
| 5 | `grep` | `async fn grep(pattern: &str, k: usize) -> Result<Vec<Hit>, LunarisError>` |
| 6 | `ls` | `async fn ls(prefix: Option<&str>) -> Result<Vec<String>, LunarisError>` |
| 7 | `forget` | `async fn forget() -> Result<ForgetReceipt, LunarisError>` |
| 8 | `as_of` | `fn as_of(ts: Hlc) -> AsOfScratchpad<'_>` |
| 9 | `AsOfScratchpad::read` | `async fn read(path: &str) -> Result<Option<String>, LunarisError>` |

A few load-bearing facts:

- **`new` is pure** — no I/O. The session prefix is `helios:fs/<session_id>/`,
  frozen by convention. The first storage round-trip happens on the first
  `write` / `read` / `grep` / `ls` / `forget`. Use a `Ulid` (or UUIDv7) for
  the session id in multi-session servers — two pads with the same id
  co-mingle and one `forget()` wipes both.
- **`write` / `read` route through `WorkingMemory`** — the content `String`
  is wrapped as `serde_json::Value::String(...)` on write and unwrapped on
  read. `read` returns `None` for "never written / already purged" (not
  `Some("")`); for large payloads that the chunker split, `read` falls back
  to a multi-chunk reconstruction path that concatenates up to 8 hits.
- **`edit` is a plain `write` of the new content.** `_old` is accepted for
  Helios's Read/Edit symmetry but unused — MVCC supersede stamps the prior
  version's `bt.sys[1]` automatically when the new ingest commits. No
  history is overwritten in place; `pad.as_of(pre_edit_ts).read(path)`
  returns the pre-edit bytes.
- **`grep` is hybrid recall** (`Vector + Keyword(BM25) + RRF + rerank` per
  `Lunaris::recall` defaults) scoped to the `helios:fs/<sid>/` prefix via
  `Filter::StartsWith` — never a SQL wildcard fragment. It surfaces
  `Hit::degraded` per hit when the verifier queue is backed up; the agent
  UX decides what to do with that flag.
- **`forget()` is soft-delete only.** It lowers to
  `ForgetTarget::Scope(ScopeSpec::BySource(session_prefix))` with default
  options — an MVCC supersede that stamps `bt.sys[1]`; rows are still
  physically present and return from `read_as_of(ts)` for any `ts` before
  the delete. There is no `pad.hard_forget()` — for GDPR-irreversible purge
  you drop to the `Lunaris` handle's two-step `confirm_hard_forget` rail.
  See [Forgetting](../guides/forget.md).
- **`as_of` returns a borrowed, read-only view.** `AsOfScratchpad<'a>` holds
  `&HeliosScratchpad` so the borrow checker stops you moving the pad while a
  time-travel view is alive. Its only method is `read(path)`. There is no
  historical `write` / `edit` / `grep` / `forget`.

Everything else — graph-aware recall, dry-run forget, hard-delete
confirmation, verifier queue tuning — drops one level to the `Lunaris`
handle itself. The recipe is intentionally narrow.

## Example — basic session lifecycle

```rust,no_run
use std::sync::Arc;
use lunaris::{HeliosScratchpad, Lunaris};

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    // One handle per process — share via Arc. URL scheme picks the backend.
    let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);

    // Session prefix becomes "helios:fs/session-42/".
    let pad = HeliosScratchpad::new(lunaris.clone(), "session-42");

    // Write two docs.
    pad.write("notes.md", "# Notes\nFirst draft.").await?;
    pad.write("todo.md",  "- [ ] finish draft").await?;

    // Read back. `read` returns `Option<String>` — `None` means no hit.
    let notes = pad.read("notes.md").await?;
    assert!(notes.is_some());

    // Edit — `_old` is accepted for symmetry but unused; MVCC supersedes
    // the prior version automatically.
    pad.edit("notes.md", "First draft.", "# Notes\nSecond draft.").await?;

    // Hybrid recall over this session's namespace.
    let hits = pad.grep("draft", 5).await?;
    for h in &hits {
        println!("source={} score={:.3} degraded={}", h.source, h.score, h.degraded);
    }

    // List stored paths (session-prefix stripped, sorted, deduped).
    let paths = pad.ls(None).await?;
    println!("session paths: {paths:?}");

    // End of session: soft-delete every primitive under the session prefix.
    let _receipt = pad.forget().await?;

    Ok(())
}
```

## Example — time-travel debugging

```rust,no_run
use std::sync::Arc;
use lunaris::{HeliosScratchpad, Hlc, Lunaris};

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("moon://localhost:6380").await?);
    let pad = HeliosScratchpad::new(lunaris.clone(), "session-42");

    // t1: agent writes the first draft.
    pad.write("plan.md", "Plan v1: go left").await?;

    // Capture a causal timestamp at the decision point. `HlcClock::tick()`
    // (via `lunaris.clock()`) is the only monotonic-now source — `Hlc::now()`
    // does not exist.
    let decision_hlc: Hlc = lunaris.clock().tick();

    // t2: agent edits the plan.
    pad.edit("plan.md", "Plan v1: go left", "Plan v2: go right").await?;

    // The live read sees the latest version.
    let latest = pad.read("plan.md").await?;
    assert_eq!(latest.as_deref(), Some("Plan v2: go right"));

    // ... but the time-travel view reads the state as-of t1.
    let as_of_view = pad.as_of(decision_hlc);
    let historical = as_of_view.read("plan.md").await?;
    assert_eq!(historical.as_deref(), Some("Plan v1: go left"));

    Ok(())
}
```

## Notes

- **Resuming a session needs no load step.** `HeliosScratchpad::new(handle,
  session_id)` is pure (no I/O) — it just builds the
  `"helios:fs/<session_id>/"` prefix. A later process that reconstructs the pad
  with the same `session_id` sees every prior `write` / `edit` (the data lives
  in the durable backend keyspace `lunaris:{scope}:{kind}:{ulid}`); a
  `pad.as_of(ts)` view still reads any historical state. Same id ⇒ same pad —
  which is also why two pads with the same id co-mingle.
- **`HeliosScratchpad` is the recipe; `WorkingMemory` is the primitive.**
  If you want a JSON-valued scratchpad rather than a string-valued
  filesystem, use [`WorkingMemory`](./index.md#workingmemory--scope-prefixed-scratchpad)
  directly. If you want consolidator promotion of hot notes, that is
  toggled per-scope on the consolidator pipeline (
  `lunaris.consolidator_pipeline()...`) — `HeliosScratchpad` itself adds no
  `consolidate` method.
- **Backend** — `moon://host:port` is the only selector as of 0.7.0. The
  latency budget is Moon recall p50 ≤ 25 ms; see
  [The Storage Backend](../operations/backends.md).
- **For everything beyond the basics** — multi-session servers, hard
  delete, graph-aware entity recall, degraded-state handling, the
  production checklist — read [`docs/helios-integration.md`](https://github.com/pilotspace/lunaris/blob/main/docs/helios-integration.md).

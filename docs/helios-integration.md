# Helios Integration Guide

Status: alpha (tracks `lunaris = "0.0.1"`; Phase 5 `HELIOS-01` / `HELIOS-02`, Phase 12 `HELIOS-03` rewrote the recipe to delegate `write` / `read` to the `WorkingMemory` primitive — the public API is byte-stable). Companion to [`docs/guide.md`](guide.md) — the user guide covers the full Lunaris surface; this document zooms into the one named v0 downstream consumer, Helios. If a claim here disagrees with the Rust source, the source wins — every symbol below carries a `path:line` cross-reference.

## How Helios talks to Lunaris

Per `helios-rfc.md` §5.3, Helios replaces deepagents' ephemeral `dict`-backed mock filesystem with a bi-temporal MVCC store. The v0 binding is the `CodingSessionMemory` recipe (`crates/lunaris/src/recipes/coding_session_memory.rs`) — a ≤50-LOC public surface wrapping `Lunaris` in the Helios file-tool contract. Helios's Read / Write / Edit / Grep / Ls / Forget tool surface maps 1:1 onto Lunaris under the frozen source prefix `helios:fs/<session_id>/<path>`; as of Phase 12 (`HELIOS-03`) `write` / `read` go through the `WorkingMemory` primitive, while `grep` / `ls` / `forget` / `as_of` stay on the direct `Lunaris` recall / storage / forget paths.

### Boundary statement

**Helios uses Lunaris; Lunaris doesn't know Helios exists.** This is enforced at the crate boundary: `lunaris` exports `CodingSessionMemory` and its borrowed `AsOfScratchpad` view (`crates/lunaris/src/lib.rs:83`) and otherwise has zero Helios-shaped types. Helios owns the tool UX, the session model, the chat-turn scheduler, and the prompt-rendering decision of whether to surface a degraded-result banner. Lunaris only sees opaque `Episode`s whose `source` happens to begin with `helios:fs/`. Dropping Helios would not require any change inside Lunaris — the recipe is a convenience, not a coupling.

See also the Out-of-Scope row in `PROJECT.md` ("Claude Code FS adapter shape (CAS, mtime, prefix_scan_meta_only) inside Lunaris") — Helios FS-tool ergonomics live in the Helios repo, not this one.

### Tool-surface mapping

**v2 (Phase 12 `HELIOS-03`):** `write` / `read` route through the Phase 9 `WorkingMemory` primitive — the `content: String` is wrapped as `serde_json::Value::String(...)` on write and unwrapped on read, so the v0.1.0 caller surface is byte-stable. Reproduced from the module docstring at `crates/lunaris/src/recipes/coding_session_memory.rs:7-15`:

| helios-rfc §5.3 | Lunaris call                                                 |
|-----------------|--------------------------------------------------------------|
| write(p, c)     | `WorkingMemory::write(p, Value::String(c))`                  |
| read(p)         | `WorkingMemory::read(p)` → unwrap `Value::String` (multi-chunk `read_at` fallback) |
| edit(p, _, n)   | `write(p, n)` — MVCC supersede via Plan 04-04 path           |
| grep(pat, k)    | `Lunaris::recall().filter(Filter::StartsWith { field: "source", prefix: "helios:fs/<sid>/" })` |
| ls(p)           | `storage().scan_range(<prefix bytes>, None)`                 |
| forget()        | `Lunaris::forget(ForgetTarget::Scope(ScopeSpec::BySource))`  |
| as_of(ts)       | borrowed view re-running `read_at` against a fixed `Hlc`     |

The ≤50-LOC public-surface contract (`HELIOS-01`) is enforced at compile time by the test `coding_session_memory_public_surface_under_50_loc` (`crates/lunaris/src/recipes/coding_session_memory.rs:290`) — which now asserts *exactly* nine public symbols (the surface may not shrink either). The nine public symbols are:

1. `CodingSessionMemory::new(Arc<Lunaris>, &str) -> Self`
2. `CodingSessionMemory::write(&self, path, content) -> Result<Lsn, LunarisError>`
3. `CodingSessionMemory::read(&self, path) -> Result<Option<String>, LunarisError>`
4. `CodingSessionMemory::edit(&self, path, _old, new) -> Result<Lsn, LunarisError>`
5. `CodingSessionMemory::grep(&self, pattern, k) -> Result<Vec<Hit>, LunarisError>`
6. `CodingSessionMemory::ls(&self, Option<&str>) -> Result<Vec<String>, LunarisError>`
7. `CodingSessionMemory::forget(&self) -> Result<ForgetReceipt, LunarisError>`
8. `CodingSessionMemory::as_of(&self, Hlc) -> AsOfScratchpad<'_>`
9. `AsOfScratchpad::read(&self, path) -> Result<Option<String>, LunarisError>`

> **v0.5 deprecation note:** `HeliosScratchpad` is available as `pub type HeliosScratchpad = CodingSessionMemory` with `#[deprecated(since = "0.5.0")]`. v0.4 consumers compile with a warning. The alias will be removed in v0.7.

Every other operation (graph recall, dry-run forget, hard-delete confirmation, verify-queue-depth tuning) drops one level to the `Lunaris` handle itself — the recipe is intentionally narrow.

### Process / session topology

```
  Helios process (one per node)
  +--------------------------------------------+
  |  Arc<Lunaris>  (one per process)           |
  |  +--------------------------------------+  |
  |  |  CodingSessionMemory #1 (session A)  |  |
  |  |   session_prefix="helios:fs/A/"      |  |
  |  +--------------------------------------+  |
  |  +--------------------------------------+  |
  |  |  CodingSessionMemory #2 (session B)  |  |
  |  |   session_prefix="helios:fs/B/"      |  |
  |  +--------------------------------------+  |
  |  +--------------------------------------+  |
  |  |  CodingSessionMemory #N (session …)  |  |
  |  +--------------------------------------+  |
  +--------------------------------------------+
                    |
                    v
           +----------------+
           |  Moon OR PG    |
           |  (URL scheme   |
           |   decides)     |
           +----------------+
```

One `Arc<Lunaris>` per process. One `CodingSessionMemory` per in-flight session. The pad holds `Arc<Lunaris>` + `session_prefix: String` + a `WorkingMemory` (itself `Arc<Lunaris>` + `String`) (`coding_session_memory.rs:80-88`) and is `Clone` — every field is cheap. Concurrency is the `Arc<Lunaris>` handle's problem; the pad adds nothing except the prefix string and the delegated primitive.

Source convention: `HELIOS_PREFIX = "helios:fs/"` (`coding_session_memory.rs:66`). Changing that constant would ripple through every downstream consumer — treat it as frozen for v0.

---

## Scenario 1 — Basic session lifecycle

### Problem

A single agent session runs end-to-end inside one process: open Lunaris, create a scratchpad for session `"session-42"`, do a handful of write/read/edit operations, purge the session when the agent disconnects.

### When to use it

- One-shot CLI runs, short-lived demos, local debugging sessions.
- Smoke tests that don't need the multi-tenant orchestration of Scenario 2.
- The first code you write when wiring Lunaris into a fresh Helios deployment.

### Code

```rust
use std::sync::Arc;
use lunaris::{CodingSessionMemory, Lunaris};

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    // One handle per process — share it via Arc. `Lunaris::open` picks the
    // backend by URL scheme (`crates/lunaris/src/handle.rs:148-206`).
    let lunaris = Arc::new(Lunaris::open("moon://localhost:6379").await?);

    // Session prefix = "helios:fs/session-42/" (coding_session_memory.rs:95).
    let pad = CodingSessionMemory::new(lunaris.clone(), "session-42");

    // Write two docs.
    let _lsn_notes = pad.write("notes.md", "# Notes\nFirst draft.").await?;
    let _lsn_todo  = pad.write("todo.md",  "- [ ] finish draft").await?;

    // Read back.
    let notes: Option<String> = pad.read("notes.md").await?;
    assert!(notes.is_some());

    // Edit — `_old` parameter is present for Helios Read/Edit symmetry but
    // unused in the implementation (coding_session_memory.rs:131-137). MVCC
    // supersedes via Plan 04-04's `apply_supersede` path — no per-recipe
    // mutation logic.
    pad.edit("notes.md", "First draft.", "# Notes\nSecond draft.").await?;

    // List — returns unique `<sid>/`-stripped paths sorted + deduped
    // (coding_session_memory.rs:162-200).
    let paths: Vec<String> = pad.ls(None).await?;
    eprintln!("session paths: {paths:?}");

    // End of session: soft-delete every primitive under `helios:fs/session-42/`.
    let _receipt = pad.forget().await?;

    Ok(())
}
```

### Gotchas

- **`forget()` is soft-delete by default.** It writes an MVCC supersede op that stamps `bt.sys[1]` on every matched primitive (`crates/lunaris/src/forget.rs:468-502`). The rows are still physically present and return from `read_as_of(ts)` for any `ts` before the soft-delete. For GDPR-irreversible purge see Scenario 6 — you must route through `Lunaris::confirm_hard_forget` (`crates/lunaris/src/forget.rs:307`), NOT through the recipe surface.
- **`edit` preserves history automatically.** The old version is never overwritten in place. A later `pad.as_of(pre_edit_ts).read("notes.md")` returns the pre-edit bytes. Scenario 5 covers the time-travel read path.
- **Session lifetime is the caller's concern.** The pad is `Clone` but carries no drop hook — if you never call `forget()`, every episode stays until a retention-bound `ForgetTarget::Before` sweep or an explicit purge. Scenario 6 walks the retention path.
- **`read` returns `Option<String>`.** `None` = zero hits (never written / already purged / empty path). Don't conflate empty-string (`Some("")`) with "no such file" (`None`). It first tries `WorkingMemory::read` (single `Value::String`); on a miss it falls back to the multi-chunk `read_at` path that concatenates up to `READ_TOP = 8` hits (`coding_session_memory.rs:71, 113-129, 247-272`).

---

## Scenario 2 — Multi-session agent server

### Problem

One Helios process serves many concurrent agent sessions — e.g., a long-running axum server where each WebSocket connection is its own session. Sessions must be fully isolated: session A's `grep` must never return session B's bytes, and `pad_a.forget()` must never touch session B's data.

### When to use it

- Production Helios deployments.
- Any server-style program where sessions outlive individual requests and incoming requests spawn per-session scratchpads.
- Per-tenant deployments where each tenant holds one or more sessions.

### Code

```rust
use std::sync::Arc;
use lunaris::{CodingSessionMemory, Lunaris};
use ulid::Ulid;

/// Process-wide state — one handle shared across every request.
struct AppState {
    lunaris: Arc<Lunaris>,
}

impl AppState {
    async fn new(url: &str) -> Result<Self, lunaris::LunarisError> {
        Ok(Self { lunaris: Arc::new(Lunaris::open(url).await?) })
    }

    /// Per-request: allocate a fresh session id, construct a scratchpad.
    /// The Arc::clone is cheap — no storage round-trip.
    fn open_session(&self) -> CodingSessionMemory {
        // Session id MUST be unique across the cluster. Ulid or UUIDv7 —
        // NOT a per-process counter (two processes would collide) and NOT
        // the user id (one user may have many sessions and want the
        // older ones purged independently).
        let session_id = Ulid::new().to_string();
        CodingSessionMemory::new(self.lunaris.clone(), &session_id)
    }
}

async fn handle_connection(state: Arc<AppState>) -> Result<(), lunaris::LunarisError> {
    let pad = state.open_session();

    // Session-scoped writes / reads. The `helios:fs/<ulid>/` prefix
    // isolates this session from every other in-flight pad.
    pad.write("scratch.md", "session-local state").await?;
    let _body = pad.read("scratch.md").await?;

    // At connection close — soft-purge this session only. The
    // `ForgetTarget::Scope(ScopeSpec::BySource(prefix))` path matches
    // by JSON `source.starts_with(prefix)` (crates/lunaris/src/forget.rs:417-421),
    // so every other pad's prefix is untouched.
    let _ = pad.forget().await?;
    Ok(())
}
```

### Gotchas

- **Uniqueness of `session_id` is Helios's responsibility.** The recipe formats `helios:fs/<session_id>/` without any sanity check (`coding_session_memory.rs:95`). If two pads choose the same id their data co-mingles — and one `forget()` wipes both. Use `Ulid::new()` (monotonic, lexicographically sortable) or UUIDv7.
- **`Arc<Lunaris>` is the sharing unit, not the pad.** The pad holds `Arc<Lunaris>` + `String` — cloning the pad is cheap but conceptually you're handing out a separate session-view object. Share the handle, construct fresh pads per session.
- **Cross-session reads require dropping to `Lunaris::recall`.** `pad.grep(...)` installs the scope filter `Filter::StartsWith { field: "source", prefix: "helios:fs/<sid>/" }` (`coding_session_memory.rs:151-156`) — never a SQL wildcard fragment (T-12-01-01 mitigation), and by construction it only hits this session's bytes. If Helios needs a cross-session view (e.g., admin debugging), it bypasses the pad and calls `lunaris.recall()` with a wider filter — `recall().filter_str("source LIKE 'helios:fs/%'")` is fine there, since `filter_str`'s v0 grammar parses `LIKE 'prefix%'` into the same `Filter::StartsWith`. See Scenario 7 for the pattern.
- **No per-pad warm-up.** `CodingSessionMemory::new` is pure (no I/O). You can create and drop pads freely without round-tripping to the backend — the first storage call happens in the first `write`/`read`/`grep`/`ls`/`forget`.
- **Backpressure is process-level.** Every pad shares the same underlying `Arc<dyn StoragePort>`, so per-session concurrency limits are Helios's orchestration layer (e.g., `tower::limit::ConcurrencyLimit`), not something the pad enforces.

---

## Scenario 3 — 10K-turn chat agent

### Problem

A long-running chat agent writes one scratchpad file per turn (`turn-000000.md` … `turn-009999.md`), reads recent turns for context, periodically edits older turns to annotate them, and periodically greps the whole session for a pattern. Recall latency must stay bounded as the session grows — a 25 ms p50 at turn 10 and a 250 ms p50 at turn 10_000 is not acceptable.

### When to use it

- Chat-agent workloads with hundreds-to-tens-of-thousands of turns per session.
- CI smoke tests of the CodingSessionMemory happy path on fresh Moon / Postgres.
- Budget-regression tracking (INGEST-05 / RETRIEVE-11 / RETRIEVE-12).

### Code

This is a paraphrase of `helios_chat_10k_turns_dual_backend` at `crates/lunaris/tests/coding_session_memory_smoke.rs:43-103`. The pattern is production-shaped: one `write` per turn, one `read` per turn, `edit + grep` every 10 turns to avoid 10× amplification.

```rust
use std::sync::Arc;
use std::time::Instant;
use lunaris::{CodingSessionMemory, Lunaris};

async fn run_chat_session(url: &str, turns: usize)
    -> Result<(f64, f64), lunaris::LunarisError>
{
    let lunaris = Arc::new(Lunaris::open(url).await?);
    let session_id = format!("smoke-chat-{}", ulid::Ulid::new());
    let pad = CodingSessionMemory::new(lunaris.clone(), &session_id);

    let mut ingest_samples_ms: Vec<f64> = Vec::with_capacity(turns);
    let mut recall_samples_ms: Vec<f64> = Vec::with_capacity(turns);

    for i in 0..turns {
        let path = format!("turn-{i:06}.md");
        let content = format!(
            "Turn {i}: hello from chat smoke. The quick brown fox jumps over the lazy dog."
        );

        // Per-turn write.
        let t0 = Instant::now();
        pad.write(&path, content.clone()).await?;
        ingest_samples_ms.push(t0.elapsed().as_secs_f64() * 1000.0);

        // Per-turn read — the scratchpad's own last-write-back path.
        let t1 = Instant::now();
        let _read = pad.read(&path).await?;
        recall_samples_ms.push(t1.elapsed().as_secs_f64() * 1000.0);

        // Edit + grep on every 10th turn. Avoids 10x amplification while
        // still exercising both code paths.
        if i % 10 == 0 && i > 0 {
            let prior = format!("turn-{:06}.md", i - 1);
            pad.edit(&prior, "hello", "HELLO").await?;
            let _ = pad.grep("brown fox", 5).await?;
        }
    }

    // Cleanup so re-runs don't accumulate session state.
    let _ = pad.forget().await?;

    ingest_samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    recall_samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let ingest_p50 = ingest_samples_ms[ingest_samples_ms.len() / 2];
    let recall_p50 = recall_samples_ms[recall_samples_ms.len() / 2];
    Ok((ingest_p50, recall_p50))
}
```

### Budgets

Sourced from `budgets()` at `coding_session_memory_smoke.rs:195-201` (which in turn cites `INGEST-05` / `RETRIEVE-11` / `RETRIEVE-12`):

| Backend    | ingest p50 | recall p50 |
|------------|-----------:|-----------:|
| Moon       |      50 ms |      25 ms |
| Postgres   |     100 ms |      60 ms |

Moon over budget is a **hard-fail** — the blueprint §4.2 differentiator is the sub-25 ms recall. Postgres inside `2×` over budget is a hard-fail; past `2×` soft-fails per Plan 02-04 D-12 — the portability backend is allowed to lag the native one, not to ship broken. See `check_budget` at `coding_session_memory_smoke.rs:206-219` for the enforcement.

### Gotchas

- **Run the full 10K target on real hardware.** The smoke test defaults to `turns = 200` — a dev-box accommodation. Set `LUNARIS_HELIOS_SMOKE_TURNS=10000` on CI / UAT to hit the documented target (`coding_session_memory_smoke.rs:57-60`).
- **Per-turn `read(&path)` is NOT the same as looking up a KV row.** It first tries `WorkingMemory::read` (a single `Value::String`); on a miss it runs the full `recall_with_degraded_check` pipeline via `read_at`, filters by `Filter::StartsWith { field: "source", prefix: "helios:fs/<sid>/<path>" }`, and concatenates up to 8 chunk hits (`coding_session_memory.rs:113-129, 247-272`). The p50 budget covers this full pipeline — ingest + chunker + embedder + vector + BM25 + RRF + rerank on the recall side. See `docs/guide.md` §3 for the full recall DSL this lowers into.
- **`edit` retains history automatically via MVCC.** Plan 04-04's `apply_supersede` stamps `bt.sys[1]` on the old chunk rows when the new ingest commits. No per-recipe mutation code — the pad's `edit` is literally `self.write(path, new)` (`coding_session_memory.rs:131-137`). If you forgot to call `forget()` at session end, every edit is still queryable via `pad.as_of(pre_edit_ts)`.
- **Recall latency is bounded by `READ_TOP = 8` chunks.** If your scratchpad documents exceed 8×500 tokens (`coding_session_memory.rs:71`) you will silently see truncated reconstructions. Increase via a wider `Lunaris::recall()` call with a larger `.top(n)` and your own scope filter — the ergonomic path doesn't expose a knob.
- **`LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` tunes the degraded flag.** Default 1000 (`crates/lunaris/src/recall.rs:26`). Set higher on workloads that run with the verifier worker off entirely — see Scenario 8.

---

## Scenario 4 — 50K-document RAG

### Problem

Before serving queries, bulk-index a large markdown corpus (documentation site, research papers, ticket archive). At runtime answer each user question by hybrid-retrieving the top `k` chunks via `pad.grep(query, k)`.

### When to use it

- Knowledge-base-backed Q&A agents.
- Any use case where the corpus is mostly static and the hot path is recall, not ingest.
- Eval-harness runs (`EVAL-05`, `EVAL-06`) that need a pre-populated corpus.

### Code

This paraphrases `helios_doc_rag_50k_md_dual_backend` at `crates/lunaris/tests/coding_session_memory_smoke.rs:109-163`. Note the deliberate source-prefix divergence — see Gotchas.

```rust
use std::sync::Arc;
use lunaris::{CodingSessionMemory, Lunaris};

async fn build_and_query(url: &str, docs: u64)
    -> Result<(), lunaris::LunarisError>
{
    let lunaris = Arc::new(Lunaris::open(url).await?);
    let session_id = format!("smoke-rag-{}", ulid::Ulid::new());
    let pad = CodingSessionMemory::new(lunaris.clone(), &session_id);

    // `build_md_doc_corpus` batches 64 markdown episodes per atomic_write
    // against the raw storage port (crates/lunaris-bench/src/corpus.rs:783).
    // Signature verified against source:
    //   pub async fn build_md_doc_corpus(
    //       storage: &dyn StoragePort,
    //       count: u64,
    //       seed: u64,
    //   ) -> Result<u64, BenchCorpusError>;
    //
    // NOTE the source convention: build_md_doc_corpus writes each episode with
    // `source = "bench:md-doc/<idx>"` — NOT `helios:fs/...`. See Gotchas.
    let written = lunaris_bench::build_md_doc_corpus(
        lunaris.storage().as_ref(),   // Arc<dyn StoragePort> -> &dyn StoragePort
        docs,
        0xDEAD_BEEF,                   // seed — determinism contract
    )
    .await
    .map_err(|e| lunaris::LunarisError::Storage(
        lunaris::StorageError::Backend(format!("bulk ingest: {e}"))
    ))?;
    eprintln!("bulk-ingested {written} docs");

    // 100 grep samples — enough to surface a meaningful p50 without
    // dominating the wall-clock on a small corpus.
    for q in 0..100u32 {
        // Because build_md_doc_corpus wrote under `bench:md-doc/`, the pad's
        // default prefix-scoped grep WILL NOT SEE THESE DOCS. This call
        // returns zero hits for every query. See Gotchas for the fix.
        let _hits = pad.grep(&format!("Section {} Lorem", q % 8), 5).await?;
    }
    Ok(())
}
```

### Gotchas

- **`build_md_doc_corpus` writes under `bench:md-doc/<idx>`, not `helios:fs/<sid>/<path>`.** This is documented at `crates/lunaris-bench/src/corpus.rs:772-775`: *"Synthetic episodes use `source = "bench:md-doc/<idx>"` so they don't collide with `helios:fs/...` Helios-namespaced data when both run on the same backend."* The pad's `grep` is scoped via `Filter::StartsWith { field: "source", prefix: "helios:fs/<sid>/" }` (`coding_session_memory.rs:151-156`), so the grep loop above will return zero hits. Two valid workarounds:
  1. **Ingest via `pad.write` under the session prefix.** Each markdown body becomes one `Episode { source: "helios:fs/<sid>/<path>", ... }` and the pad's `grep` finds it. Slower at ingest (no 64-wide batching) but data stays inside the pad abstraction.
  2. **Drop to `lunaris.recall()` with a wider filter.** Bypass the pad for retrieval:
     ```rust
     let hits = lunaris.recall()
         .filter_str("source LIKE 'bench:md-doc/%'").unwrap()
         .top(5)
         .execute(lunaris_retrieve::Query::text("Section 0 Lorem"))
         .await?;
     ```
     This is what the smoke test does implicitly via `pad.grep(...)` — the zero-hit result does not cause it to fail because the budget check is a *latency* check, not a *correctness* one. See `check_budget` at `coding_session_memory_smoke.rs:206-219`.
- **`LUNARIS_HELIOS_SMOKE_DOCS=50000` unlocks the full target.** Default `docs = 1_000` for dev-box runs (`coding_session_memory_smoke.rs:123-126`). The 50K document pass takes ~5 minutes on warm Moon / Postgres.
- **Cleanup is the operator's problem on the bulk path.** Because the bulk corpus doesn't land under `helios:fs/<sid>/`, calling `pad.forget()` won't touch it. The smoke test skips cleanup and relies on a fresh backend per CI run (`coding_session_memory_smoke.rs:156-161`). In your production harness, call `lunaris.forget(ForgetTarget::Scope(ScopeSpec::BySource("bench:md-doc/".into())))` explicitly, or use a scoped session.
- **`storage().as_ref()` is the shape `build_md_doc_corpus` wants.** The helper takes `&dyn StoragePort`; `Lunaris::storage()` returns `Arc<dyn StoragePort>` (`crates/lunaris/src/handle.rs:369-371`); `.as_ref()` bridges the two. This is the exact invocation at `coding_session_memory_smoke.rs:129-133` — copy it verbatim.

---

## Scenario 5 — Time-travel debugging

### Problem

An agent made a wrong decision at turn 847. The user wants to see what the agent's scratchpad looked like at that moment — specifically, the contents of `plan.md` before the agent edited it at turn 850.

### When to use it

- Post-incident debugging of agent behaviour.
- Audit replay to explain a decision after-the-fact.
- Regression tests that pin expected output at a prior revision.

### Code

```rust
use std::sync::Arc;
use lunaris::{CodingSessionMemory, Hlc, Lunaris};

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("moon://localhost:6379").await?);
    let pad = CodingSessionMemory::new(lunaris.clone(), "session-42");

    // t1: agent writes the first draft.
    pad.write("plan.md", "Plan v1: go left").await?;

    // Capture the HLC at the decision point.
    //
    // `HlcClock::tick()` is the only monotonic-now source in the codebase —
    // `Hlc::now()` does NOT exist (see the B-4 note at
    // crates/lunaris/src/forget.rs:16). The handle borrows the clock via
    // `Lunaris::clock()` (crates/lunaris/src/handle.rs:378-380).
    let decision_hlc: Hlc = lunaris.clock().tick();

    // t2: agent edits the plan.
    pad.edit("plan.md", "Plan v1: go left", "Plan v2: go right").await?;

    // t3: the edited version is what `pad.read` sees now.
    let latest: Option<String> = pad.read("plan.md").await?;
    assert_eq!(latest.as_deref(), Some("Plan v2: go right"));

    // ... but the time-travel view reads the state as-of t1.
    //
    // `pad.as_of(ts)` returns an `AsOfScratchpad<'_>` — a borrowed view
    // that cannot outlive `pad` (coding_session_memory.rs:215-237).
    let as_of_view = pad.as_of(decision_hlc);
    let historical: Option<String> = as_of_view.read("plan.md").await?;
    assert_eq!(historical.as_deref(), Some("Plan v1: go left"));

    Ok(())
}
```

### Gotchas

- **`AsOfScratchpad` is a borrowed view.** It holds `&CodingSessionMemory` (`coding_session_memory.rs:225-228`), so the borrow checker will stop you from moving the pad while an `as_of` view is alive. Good — this is the intentional invariant. Don't try to store the view in a long-lived struct.
- **Time-travel is read-only.** `AsOfScratchpad` exposes one method — `read(path)` (`coding_session_memory.rs:233-236`). There is no `write`, `edit`, `forget`, or `grep` at a historical timestamp. If you need a historical write (i.e., compensating rewrite), you issue a fresh `pad.write()` now and let MVCC stamp the current time.
- **Capture the HLC, not wall-clock time.** `HlcClock::tick()` returns a causal timestamp bound to the handle's monotonic counter (`crates/lunaris-core/src/hlc.rs:41-57`). Using `std::time::SystemTime::now()` and parsing it into an `Hlc` is wrong — the HLC has a node id and a counter; two HLCs with different node ids can compare equal at the `wall_ms` field while being distinct causal points.
- **The `Hlc` the backend stamps at ingest is NOT the `clock.tick()` you observe at the call site.** The bi-temporal coordinates are stamped deep in the ingest pipeline (the chunker), not at the recipe boundary — `CodingSessionMemory::write` just forwards the content to `WorkingMemory::write` (`crates/lunaris/src/recipes/coding_session_memory.rs:104-106`), which routes through `Lunaris::ingest`. Capture your decision HLC from `lunaris.clock().tick()` at your own call site, which will be a causal successor to all the episodes the handle has ingested up to that point.
- **`as_of` does not bypass `forget`.** A soft-delete (`pad.forget()`) also records a `bt.sys[1]` timestamp at the moment of deletion; `as_of(t)` for `t` prior to the forget still returns the content, but the bi-temporal model is valid-time-based. See `docs/guide.md` §5 for the full bi-temporal semantics.

---

## Scenario 6 — GDPR purge / session expiry

### Problem

A user closes their account and legal requires every trace of their data gone within N days — soft-delete is not enough. Alternatively: a session has been idle longer than the retention window and should be swept.

### When to use it

- GDPR / CCPA deletion requests.
- Automated retention sweeps.
- Compliance audits where the `__lunaris_audit__` topic becomes the source of truth for what was purged when.

### Code

```rust
use std::sync::Arc;
use lunaris::{ForgetTarget, CodingSessionMemory, Lunaris, ScopeSpec};

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("moon://localhost:6379").await?);
    let pad = CodingSessionMemory::new(lunaris.clone(), "session-42");

    // --- SOFT path ----------------------------------------------------------
    // Reversible via `read_as_of(ts_before_forget)`. Good for session expiry
    // where you might want to resurrect. This is what the recipe's
    // `pad.forget()` does under the hood (coding_session_memory.rs:206-210).
    let _soft_receipt = pad.forget().await?;

    // --- HARD path ----------------------------------------------------------
    // D-21 two-step rail enforced by
    // `LunarisError::Validate(ValidateError::ConfirmationRequired(_))`
    // (crates/lunaris/src/forget.rs:233-237). Also see the behaviour matrix
    // at forget.rs:200-205.
    let session_prefix = format!("helios:fs/{}/", "session-42");
    let target = ForgetTarget::Scope(ScopeSpec::BySource(session_prefix.clone()));

    // Step 1: dry-run preview — returns a receipt with `preview=true`,
    // no atomic_write issued (crates/lunaris/src/forget.rs:244-260).
    let preview = lunaris.forget(target.clone().dry_run()).await?;
    assert!(preview.preview);
    eprintln!(
        "preview: would touch {} rows (soft) or delete {} rows (hard)",
        preview.rows_written, preview.rows_deleted,
    );

    // Step 2: exchange the preview receipt for a confirmation token
    // (crates/lunaris/src/forget.rs:307-317). Non-preview receipts are
    // rejected with `ConfirmationRequired`.
    let token = lunaris.confirm_hard_forget(preview).await?;

    // Step 3: commit the hard delete with the token attached. This issues
    // `WriteOp::KvDelete` ops — irreversible (forget.rs:264-271).
    let final_receipt = lunaris
        .forget(target.hard().with_token(token))
        .await?;
    assert!(!final_receipt.preview);
    assert_eq!(final_receipt.rows_written, 0);
    eprintln!("hard-deleted {} rows", final_receipt.rows_deleted);

    Ok(())
}
```

### Gotchas

- **Soft vs hard is about MVCC vs `KvDelete`, not about audit.** Every successful `forget` call publishes exactly one `AuditEvent::Forget(receipt)` to the `__lunaris_audit__` topic (`crates/lunaris/src/forget.rs:254-255, 289-290`). Soft and hard leave identical audit rows — the difference is whether the underlying KV bytes are still present or gone.
- **Hard-delete without a token fails *loudly*.** `LunarisError::Validate(ValidateError::ConfirmationRequired(_))` (`forget.rs:233-237`) is the typed variant, NOT an `anyhow::Error`. Match on it explicitly if your calling code needs to distinguish "missing confirmation" from "genuine backend failure":
  ```rust
  match lunaris.forget(target.clone().hard()).await {
      Err(lunaris::LunarisError::Validate(
          lunaris::ValidateError::ConfirmationRequired(msg),
      )) => eprintln!("need dry_run first: {msg}"),
      other => { other?; }
  }
  ```
- **The recipe surface is soft-only.** `CodingSessionMemory::forget()` always lowers to `ForgetTarget::Scope(ScopeSpec::BySource(session_prefix))` with default options (`coding_session_memory.rs:206-210`). There is no `pad.hard_forget()` — you must drop to the `Lunaris` handle as above. This is intentional: a `pad.` method that could irreversibly delete data would be a footgun in agent code-gen paths.
- **`ScopeSpec::BySource` is a prefix match on the JSON `source` field.** It is NOT regex, glob, or substring (`crates/lunaris/src/forget.rs:417-421`). Passing `"helios:fs/session-42"` without the trailing `/` would also match `helios:fs/session-42-extra/` — always include the terminator.
- **Write once, delete many.** The `forget` call issues a **single** `atomic_write` (D-19 invariant at `forget.rs:275-279`) regardless of how many rows matched the scope. Memory is bounded by `scan_range` + `read_as_of` results held briefly in a `Vec<ForgetMatch>`. For pan-tenant sweeps of millions of rows, partition the scope into per-day `Before(hlc)` sweeps instead.
- **`before` is a separate target, not a scope modifier.** `ForgetTarget::Before(hlc)` is the retention-sweep path (`forget.rs:56-58`). Combining "this session AND before this date" requires two calls, not one fused target — v0 scope language is intentionally small (`forget.rs:60-72`).

---

## Scenario 7 — Graph-aware entity recall

### Problem

The agent asks *"tell me everything you know about Alice"*. Pure vector recall returns snippets mentioning Alice, but the user wants relationship traversal: Alice works at Acme, Acme acquired Beta, Alice was also mentioned in the Beta ticket thread — two hops out. The scratchpad `pad.grep` path doesn't expose graph ops.

### When to use it

- Entity-centric queries where relationships matter.
- n-hop graph questions (1- or 2-hop typical for v0; max 3 per `MAX_GRAPH_HOPS`).
- Any case where vector + BM25 RRF is insufficient and you specifically want Cypher BFS fused in.

### Code

The CodingSessionMemory public surface is intentionally narrow (`coding_session_memory.rs:17-32`) — 9 public symbols, zero graph ops. Graph-aware recall drops to the `Lunaris` handle directly.

```rust
use std::sync::Arc;
use lunaris::{EntityId, Graph, CodingSessionMemory, Lunaris, Query, Vector};

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("moon://localhost:6379").await?);

    // (1) Turn the graph pipeline on BEFORE ingest. Default is OFF per
    //     blueprint §5.2 — `handle.graph_pipeline().is_enabled()` returns
    //     false until this flip (crates/lunaris/src/handle.rs:444,
    //     umbrella re-export at lib.rs:67).
    lunaris.graph_pipeline().enable();

    // (2) (Re-)ingest the session's episodes so the extractor runs and
    //     populates entities + relations. Retrofitting graph on an already-
    //     ingested corpus without re-ingest leaves zero graph edges —
    //     extraction happens inside the ingest hot path, not after the fact.
    let session_id = "session-42";
    let pad = CodingSessionMemory::new(lunaris.clone(), session_id);
    pad.write("alice-note.md",
        "Alice joined Acme in 2021. Acme acquired Beta the next year.").await?;

    // (3) Compose the graph-aware recall at the Lunaris handle level. This
    //     is the canonical compose example documented at
    //     crates/lunaris/src/recall.rs:62-72.
    let session_prefix = format!("helios:fs/{session_id}/");
    let alice = EntityId::from_name_and_type("Alice", "Person");

    let hits = lunaris
        .recall()
        .with_root(
            Vector::new("chunks", 30)
                .and(Graph::anchored(vec![alice], 2))
                .fuse_rrf(60)
                .rerank(lunaris.reranker())
                .top(5),
        )
        .filter_str(&format!("source LIKE '{session_prefix}%'"))
        .map_err(|e| lunaris::LunarisError::Storage(
            lunaris::StorageError::Backend(format!("filter parse: {e}"))
        ))?
        .execute(Query::text("Tell me about Alice"))
        .await?;

    for h in &hits {
        eprintln!("{:?} source={} score={}", h.source_op, h.source, h.score);
    }
    Ok(())
}
```

### Gotchas

- **Graph pipeline is OFF by default.** The single-switch surface is `handle.graph_pipeline().enable()` / `.disable()` / `.is_enabled()` (`crates/lunaris/src/handle.rs:444`). The env equivalent is `LUNARIS_GRAPH_ENABLED=1` (`crates/lunaris/src/lib.rs:67` — `GRAPH_ENABLED_ENV_VAR`). When OFF, the extractor is dead code and ingest stays in the no-graph budget (`INGEST-05`).
- **Extractor default is Candle Gemma-3 4B.** Weights load from the default cache — if they are missing the pipeline falls back to `NoopExtractor` (`lib.rs:96`). See `docs/guide.md` §4 for the Extractor trait and how to swap to `OllamaExtractor` or `CloudApiExtractor` (both umbrella-re-exported under feature flags at `lib.rs:102-107`).
- **Retrofitting graph on an already-ingested corpus requires re-ingest.** Toggling `enable()` after ingest does not back-fill entities — the extraction step runs inside `Lunaris::ingest`, not as a background sweep. In production you either (a) ingest with graph ON from day zero or (b) replay the session through `pad.write(path, content)` after flipping the toggle.
- **`Graph::anchored` requires resolvable `EntityId`s.** Use `EntityId::from_name_and_type` (`crates/lunaris-extract/src/types.rs:60`) for the canonical hash. Passing an unresolved name is a silent zero-hit — there is no "find-nearest-entity" fallback at the operator level.
- **`with_root` replaces the default root.** `Lunaris::recall()` seeds `with_root(Vector::new("chunks", 30))` by default (`crates/lunaris/src/recall.rs:73-79`); calling `.with_root(...)` again overrides it with your composed operator. Don't combine both.
- **Re-rank is separate from fuse.** `.fuse_rrf(60)` combines heterogeneous scores via reciprocal rank fusion; `.rerank(reranker)` is the cross-encoder pass (`bge-reranker-v2-m3` by default, `Reranker` trait at `lunaris-rerank`). The two are independent layers — you can have one, both, or neither. See `docs/guide.md` §4 for the full DSL.
- **`filter_str` returns `Result<RetrievalBuilder, FilterParseError>`.** Unlike `top(n)`, it can fail (bad predicate syntax). Map it into `LunarisError::Storage(StorageError::Backend(...))` as above — or propagate the parse error up if your code already carries a richer error type.

---

## Scenario 8 — Degraded-state handling

### Problem

The verifier queue (`__lunaris_verify__`) has backed up under ingest pressure — recall hits include chunks derived from extractions that have not been verified yet. The agent UX should either warn the user or apply a fallback strategy. The query must not fail.

### When to use it

- High ingest-volume production (e.g., log archives, firehose-style corpora).
- Any deployment where the verifier worker is intentionally off and the consumer needs to know.
- Operational health dashboards.

### Code

```rust
use std::sync::Arc;
use lunaris::{CodingSessionMemory, Lunaris};
use lunaris_retrieve::Hit;

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    let lunaris = Arc::new(Lunaris::open("moon://localhost:6379").await?);
    let pad = CodingSessionMemory::new(lunaris.clone(), "session-42");

    // `pad.grep` already lowers to `recall_with_degraded_check().execute(...)`
    // under the hood (coding_session_memory.rs:151-156). The degraded flag is
    // surfaced on each returned Hit.
    let hits: Vec<Hit> = pad.grep("brown fox", 5).await?;

    let any_degraded = hits.iter().any(|h| h.degraded);
    if any_degraded {
        // Helios chooses the UX — Lunaris only flags. E.g., render a
        // banner, log a metric, fall back to a cached answer, etc.
        eprintln!("recall degraded — verifier queue depth over threshold");
    }
    for h in &hits {
        eprintln!(
            "{}: score={:.3} degraded={} rerank_applied={}",
            h.source, h.score, h.degraded, h.rerank_applied,
        );
    }
    Ok(())
}
```

### Tuning

- `LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` — env knob (`crates/lunaris/src/recall.rs:26, 108-111`). Default `1000`. Set higher if you routinely run with the verifier off and don't want every `grep` call to flag `degraded=true`.
- The threshold is read **per recall call**, not cached — you can tune it live without restarting the process.

### Gotchas

- **`degraded` is a `Hit`-level boolean flag**, not a per-call exception. Every `Hit` in the returned `Vec` carries its own `degraded: bool` (`crates/lunaris-retrieve/src/types.rs:87-92`). A partial-degrade (some hits from a lagging index, some fresh) is representable. The `recall_with_degraded_check` path seeds the builder's `initial_degraded` based on queue depth at call start (`crates/lunaris/src/recall.rs:99-139`); per-hit degrade from `degraded_fallback` operators composes on top.
- **Backends without `queue_depth` fall through as non-degraded.** `StoragePort::queue_depth` is an additive method (`crates/lunaris-core/src/...`). Older backends return `Err(StorageError::NotSupported(_))`, which the recall path catches and logs at `debug` (`recall.rs:121-128`). This is best-effort observability — a silent "no queue introspection available" does NOT fail the recall.
- **`Hit::degraded` is an advisory, not a correctness claim.** An unverified extraction is still a real extraction — it just has not passed the two-model validator (`VERIFY-04` contradiction arbitration). The agent-side decision (surface a warning, re-query, ignore) is Helios's call.
- **`pad.grep` is the only scratchpad method that surfaces degrade.** `pad.read`'s multi-chunk fallback (`read_at`) also lowers to `recall_with_degraded_check` (`coding_session_memory.rs:113-129, 247-272`) but aggregates chunks into a `String` — the per-hit flag is lost in the concatenation. If you care about degrade in the `read` path, call `lunaris.recall_with_degraded_check().await?` directly and inspect hits before flattening.
- **VERIFY-V1 will enable the worker by default.** Today (v0) the verifier worker is scaffolded but off; the warn threshold is a guard against a deployment that turns it on and then falls behind. Keep the threshold tuned for expected queue depth — a too-low threshold flags every recall, a too-high one masks real lag.

---

## Scenario 9 — Dual-backend portability

### Problem

Your dev laptop runs Postgres (easy `brew install`). Staging runs Moon (we built Moon; we want to see the native `FT.*` speedups). Production is Moon-only. The Helios code must not branch on the backend — only the URL scheme changes.

### When to use it

- Local development against Postgres, CI against both, production against Moon.
- Portability proofs for customers / auditors who want to see Lunaris isn't Moon-locked.
- Any deployment that exercises `STORE-07` AS_OF parity across the two backends.

### Code

Paraphrase of the dual-backend probe at `crates/lunaris/tests/coding_session_memory_smoke.rs:43-103`. The pattern is one `for url_env in ["MOON_URL", "PG_URL"]` loop; the rest of the code is identical across backends.

```rust
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use lunaris::{CodingSessionMemory, Lunaris};

/// TCP probe with 1s timeout. Mirror of `probe_backend` at
/// coding_session_memory_smoke.rs:171-190. Accepts hostnames and literal IPs
/// (to_socket_addrs, not parse::<SocketAddr>).
fn probe_backend(env_name: &str) -> Option<String> {
    let url = std::env::var(env_name).ok()?;
    let host_port = url
        .strip_prefix("moon://")
        .or_else(|| url.strip_prefix("redis://"))
        .or_else(|| url.strip_prefix("postgres://"))
        .or_else(|| url.strip_prefix("postgresql://"))
        .unwrap_or(&url);
    let host_port = host_port.rsplit('@').next().unwrap_or(host_port);
    let host_port = host_port.split('/').next().unwrap_or(host_port);
    let timeout = Duration::from_secs(1);
    let addr = host_port.to_socket_addrs().ok().and_then(|mut it| it.next())?;
    if TcpStream::connect_timeout(&addr, timeout).is_ok() {
        Some(url)
    } else {
        eprintln!("SKIP {env_name} (TCP probe to {host_port} failed)");
        None
    }
}

#[tokio::main]
async fn main() -> Result<(), lunaris::LunarisError> {
    for url_env in ["MOON_URL", "PG_URL"] {
        let Some(url) = probe_backend(url_env) else {
            continue;
        };
        eprintln!("=== backend: {url_env} ({url}) ===");

        // Identical code path on both backends.
        let lunaris = Arc::new(Lunaris::open(&url).await?);
        let pad = CodingSessionMemory::new(lunaris.clone(), "portability-demo");

        pad.write("hello.md", "hello from dual-backend").await?;
        let body = pad.read("hello.md").await?;
        assert!(body.is_some());

        let hits = pad.grep("hello", 5).await?;
        eprintln!("{url_env} grep hits: {}", hits.len());

        let _ = pad.forget().await?;
    }
    Ok(())
}
```

### Gotchas

- **URL scheme is the only backend selector.** `moon://host:port` routes to `MoonStorage`; `postgres://user:pass@host/db` and `postgresql://...` route to `PostgresStorage` (`crates/lunaris/src/handle.rs:148-206`). Anything else returns `LunarisError::Storage(StorageError::UnsupportedScheme(_))`. No second argument, no feature flag flip — the URL wins.
- **Moon hits the native `FT.*` RRF path; Postgres does client-side RRF.** When both branches of a `fuse_rrf` land on the Moon backend, the Phase 1.5 RRF dispatch takes one round-trip via `client.text().hybrid_search()` (`STORE-09` in `REQUIREMENTS.md`). Postgres falls back to client-side RRF — correct but slower. No code change; only latency differs.
- **Latency budgets differ per backend.** Moon: `ingest p50 ≤ 50 ms, recall p50 ≤ 25 ms`. Postgres: `ingest p50 ≤ 100 ms, recall p50 ≤ 60 ms`. See Scenario 3's budget table and `budgets()` at `coding_session_memory_smoke.rs:195-201`. CI asserts these on every push.
- **Probe failure is `continue`, not `panic`.** The `probe_backend` helper silently skips a backend when its env var is unset OR the TCP probe fails — and a subsequent `eprintln!("SKIP ...")` documents why. The remaining backend still runs. This is the discipline Plan 04-03 locked in; mirror it in any custom harness.
- **AS_OF parity is a `STORE-07` contract, not an accident.** The conformance suite (`crates/lunaris-conformance`) asserts `hits_moon == hits_postgres` field-by-field for AS_OF queries. If your Helios code depends on subtle ordering, it works on both backends. If the conformance suite flags divergence in v1, the `StorageCapabilities` surface at `crates/lunaris-core/src/...` will gate it — but today (v0) parity holds across the six primitives.
- **`build_md_doc_corpus` is bench-crate-scoped, not backend-specific.** It calls `storage.atomic_write` — which every backend implements (`STORE-01`). If your integration tests vendor bench helpers, the helper works on Moon and Postgres alike. No backend-specific bulk-ingest shim exists, or is needed.

---

## Production checklist

Before shipping Helios-on-Lunaris to production, confirm each item below. These are the boundary conditions the smoke tests enforce structurally; CI enforces them on every push.

- [ ] **Session id uniqueness.** Use `Ulid::new().to_string()` or UUIDv7 — NOT a per-process counter. Two pads with the same id co-mingle and one `forget()` purges both. Scenario 2.
- [ ] **Forget strategy.** Decide per-tenant / per-session which of (soft `pad.forget()`, hard `lunaris.confirm_hard_forget + forget(...hard().with_token(...))`, temporal sweep `ForgetTarget::Before(hlc)`) you run and on what schedule. Audit events fire on every call — treat `__lunaris_audit__` as the compliance source of truth. Scenario 6.
- [ ] **Degraded observability wired.** `LUNARIS_VERIFY_QUEUE_WARN_THRESHOLD` tuned for expected queue depth; UI / metric surfaces `Hit::degraded` so operators can see lag before an outage. Scenario 8.
- [ ] **Verifier + consolidator toggles explicit.** Both default OFF in v0. If you turn on the verifier (`VerifierPipelineHandle::enable()` — umbrella at `crates/lunaris/src/lib.rs:84`), monitor `__lunaris_verify__` queue lag per `VERIFY-05`. Same for `ConsolidatorPipelineHandle`. Keep toggles in one place (startup config), not scattered across request-path code.
- [ ] **Moon vs Postgres tuning.** In production the default path is Moon — its latency budgets are the load-bearing `Core Value` from `PROJECT.md`. Postgres is the portability proof and runs at a relaxed budget (`2×` over hard-fails, beyond that soft-fails per Plan 02-04 D-12). If your CI runs dual-backend, don't treat a Postgres soft-fail as a Moon regression.
- [ ] **Hard-delete audit trail archived.** `__lunaris_audit__` events for hard forgets are the only forensic record after the KV rows are gone. Persist the topic to cold storage for the retention window your compliance regime requires.
- [ ] **Graph pipeline posture explicit.** If Helios depends on graph-aware recall (Scenario 7), ingest with graph ON from day zero — retrofitting requires re-ingest. If Helios does NOT need graph (pure RAG workloads), keep it OFF to stay inside the no-graph ingest budget.
- [ ] **Docs cross-link up to date.** This guide complements [`docs/guide.md`](guide.md) — the user guide covers the full Lunaris DSL, worker wiring, and HTTP wire protocol. When Lunaris ships a new recipe or changes the `CodingSessionMemory` surface, update this file alongside the Phase-5 plans — the nine public methods enforced by `coding_session_memory_public_surface_under_50_loc` are your contract with every downstream Helios build.

For the full Lunaris surface — installation, the recall DSL, background workers, HTTP spec, troubleshooting — see [`docs/guide.md`](guide.md). For MemoryProtocol 0.1 (the portable HTTP wire spec implemented by `lunaris-server`), see `docs/protocol/memoryprotocol-0.1.md`.

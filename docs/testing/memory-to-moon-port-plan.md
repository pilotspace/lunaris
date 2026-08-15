# `memory://` → Moon test port plan (0.7.0 prerequisite)

0.7.0 deletes the embedded SQLite backend — the `memory://` and `sqlite:///path`
URL schemes and the whole `lunaris-storage-embedded` crate. **60 `.rs` files
under `crates/` open storage through it today**, so the deletion is gated on
those files having somewhere else to go.

This document is the inventory, the recipe, and the LEDGER. Slice 1 built the
seam and ported three representative files. **Slice 2 is complete**: every
portable file is ported, and every file that could not be ported is recorded
below with the reason it could not.

---

## 1. The seam

`crates/lunaris-test-harness` hands out a **real, disposable, single-shard Moon**
— one child process per fixture — and degrades to `memory://` where no Moon
binary exists, so `cargo test --workspace` stays green on a machine that never
built the submodule.

### Why a child process, not `embedded-moon`

`lunaris-memory-service`'s `embedded-moon` feature links the Moon **server**
crate into the calling binary. CLAUDE.md keeps it out of every default feature
set precisely so `cargo test --workspace` and CI clippy never compile it. A
harness reaching for that feature would re-import the cost into every test target
in the workspace. Spawning a prebuilt binary keeps the invariant intact: no test
binary ever links Moon.

### Why one Moon per fixture, not one per test binary

Measured boot-to-RESP-ready on an M-series Mac:

| flags | boot |
|---|---|
| `--appendonly yes --appendfsync always` (the drill script's) | 24–46 ms |
| `--appendonly no` | 6.3 ms |
| `--appendonly no --maxmemory 512m --pagecache-size 64mb` (the harness's) | **2.7 ms** |

At ~3 ms, sharing is pure downside. Per-fixture is also the only arrangement that
faithfully replaces `memory://`, which is a fresh, empty, process-private
database on every `connect` — and it sidesteps the leak a shared instance would
cause, since a `static` guard's `Drop` never runs at process exit.

### Safety rails

- Ports are OS-assigned (`bind 127.0.0.1:0`, read back, release) and re-rolled off
  `RESERVED_PORTS = [6379, 6380, 6381, 6399]`, so the live memory store (6381)
  and the dedicated bench Moon (6399) can never be bound, written, or shut down.
- Data directories live under `std::env::temp_dir()` and are removed in `Drop`,
  which also `SIGKILL`s and reaps the child. `Drop` runs on unwind, so a
  panicking test leaves no orphan.
- Only a PID the fixture spawned is ever signalled. No `FLUSHALL`, anywhere.

### Binary resolution

1. `$MOON_TEST_BINARY`
2. `<workspace-root>/vendor/moon/target/release/moon`
3. `<workspace-root>/vendor/moon/target/debug/moon`

Lunaris refuses Moon `< 0.8.5` at connect, so the binary must come from the
pinned submodule (`cargo build --release --bin moon` inside `vendor/moon`). An
older binary fails loudly with a version-guard error, never subtly.

> **Gotcha — linked git worktrees.** The root manifest's `exclude` entry is
> `vendor/moon`, resolved against the *real* repo root. Inside a linked worktree
> the submodule sits at `.claude/worktrees/<name>/vendor/moon`, which the outer
> workspace then claims, and `cargo build` there dies with "current package
> believes it's in a workspace when it's not". Build the binary from the primary
> checkout (or an out-of-tree copy) and point `$MOON_TEST_BINARY` at it. Path #2
> of the resolution order above is therefore only exercised in a normal checkout.

### Backend policy

| `LUNARIS_TEST_BACKEND` | behaviour |
|---|---|
| unset / `auto` | Moon when a binary resolves, else `memory://` |
| `moon` | Moon or **panic** — the CI gate; never silently skips |
| `memory` | always `memory://` |

Every knob is also an explicit `Policy` argument. Mutating the environment is an
`unsafe fn` in edition 2024 (forbidden in this crate) and tests in one binary
share a process, so env is read once at the top and never written.

**CI should set `LUNARIS_TEST_BACKEND=moon`** on the job that has the binary. A
lost binary then surfaces as a failure instead of a silent downgrade to the very
backend this whole exercise exists to delete.

### API

```rust
open_test_engine()                            -> TestEngine   // StubEmbedder(768)
open_test_engine_with_embedder(embedder)      -> TestEngine
open_test_engine_with(policy, embedder)       -> TestEngine
open_test_storage()                           -> TestStorage  // bare Arc<dyn StoragePort>
open_test_storage_with(policy, dim)           -> TestStorage
open_test_store()  / open_test_store_with(p)  -> TestStore     // just the URL + guard
EphemeralMoon::spawn()                        -> Result<EphemeralMoon>
moon_binary()                                 -> Option<PathBuf>
```

`TestEngine` derefs to `Lunaris` and `TestStorage` to `Arc<dyn StoragePort>`, so
most call sites need no edit beyond the constructor. **Bind the return value to a
local** — it owns the Moon child, and `let _ = ...` would kill it immediately.

---

## 2. Behavioural differences met while porting

These are real. They are not harness bugs. Items 1-4 were predicted before
slice 2; items 5-8 were found by RUNNING the suite under
`LUNARIS_TEST_BACKEND=moon`, not by inspection, and each one cost a
reclassification.

1. **Recall against a never-written Moon errors.** `no temporal snapshot
   registered for the given AS_OF timestamp`, where the embedded backend returns
   an empty result set. Any test whose assertion is "recall finds nothing" must
   seed the store with something irrelevant first. See
   `two_fixtures_do_not_share_state` in `tests/harness_contract.rs`.
2. **`keyword_search` works on Moon and is `NotSupported` on the embedded
   backend.** Several tests assert the *degraded* vector-only fallback path
   explicitly. Those assertions change meaning on Moon — re-read the doc comment
   before swapping, and keep the degrade pin by forcing `Policy::ForceMemory`
   only while the embedded backend still exists (it will not after 0.7.0; those
   pins must be re-expressed against a stubbed `KeywordPort` instead).
3. **`graph_native` / `queue_native` are true on Moon, false on embedded.** Tests
   that exercise entity/relation/fact writes were previously landing plain KV
   rows; on Moon they go through the real graph path. This is an *upgrade* in
   coverage but can surface latent assumptions.
4. **Vector dim is sticky on Moon.** `FT.CREATE` fixes the index width and does
   NOT resize. A fixture whose embedder is not 768-d must pass its dim through
   `open_test_engine_with_embedder` (which reads `embedder.dim()`) or
   `open_test_storage_with(policy, dim)`.
5. **`as_of` is rejected outright on Moon.** Not "silently wrong" — an explicit
   `InvalidInput("as_of requires a bi-temporal backend; this backend reads
   current state only (Moon AS_OF parity is tracked as STORE-07)")`. Any
   bi-temporal assertion is unreachable there.
6. **Recall SCORE SCALES are not comparable across backends.** The embedded
   backend returns raw brute-force cosine (an exact match is ~1.0); Moon returns
   RRF-fused scores (~0.0164 at rank 1). Any assertion on an absolute score
   window is embedded-only by construction. Ratios survive the move; absolutes
   do not.
7. **`VectorHit.metadata` is empty on Moon's unfiltered path.**
   `lunaris-storage-moon::vector::vector_search` hydrates metadata from KV only
   on the post-filter branch (`vector.rs` ~:92-120). With `filter = None` it
   keeps whatever the FT reply carried, which for the `communities` index is
   nothing — so downstream BM25 content extraction sees no `summary`. This is a
   Moon-side gap, not a fixture problem, and is the one finding here that points
   at production code rather than at a test.
8. **Constructor choice is load-bearing for resolver tests.** `open_test_engine()`
   injects the harness's own `StubEmbedder`. Any test whose subject is the
   EMBEDDER RESOLVER (`optional_embedder.rs`, `llamacpp_wired.rs`,
   `remote_llm_wired.rs`) must take only the URL — `open_test_store()` — and
   keep calling `Lunaris::open` itself, or it passes vacuously.

---

## 3. Inventory — all 60 files (LEDGER)

Counts are `memory://` **code** sites (doc-comment mentions excluded).
Status: **DONE** ported · **PIN** ported through the harness but pinned to
`Policy::ForceMemory` with a reason · **KEEP** left on the embedded backend,
not portable · **DELETE** goes with the crate in 0.7.0.

### DONE — slice 1 (3 files)

| file | shape |
|---|---|
| `crates/lunaris/tests/structured_ingest.rs` | heavy ingest + agent-supplied graph |
| `crates/lunaris/tests/forget_scoped_moon.rs` | forget / audit receipts |
| `crates/lunaris-memory-service/tests/episode_grain_loop.rs` | recall → activation ledger → boosted re-recall |

### DELETE, do not port (7 files, 17 sites)

These test the embedded backend itself, or are the backend. They go with the
crate. Untouched by slice 2.

| file | sites | status |
|---|---|---|
| `crates/lunaris-storage-embedded/src/lib.rs` | 4 | DELETE |
| `crates/lunaris-storage-embedded/src/schema.rs` | 0 | DELETE |
| `crates/lunaris-storage-embedded/tests/vector_search.rs` | 5 | DELETE |
| `crates/lunaris-storage-embedded/tests/categories_membership.rs` | 3 | DELETE |
| `crates/lunaris-storage-embedded/tests/concurrent_handles.rs` | 3 | DELETE |
| `crates/lunaris-conformance/tests/run_storage_embedded.rs` | 1 | DELETE |
| `crates/lunaris/src/open.rs` | 1 (the `"memory" \| "sqlite"` dispatch arm) | DELETE |

### PRODUCTION code — a decision, not a port (1 file, 1 site)

| file | site | status |
|---|---|---|
| `crates/lunaris-bench/src/eval/er_f1.rs:98` | `.unwrap_or_else(\|_\| "memory://".to_string())` — the ER-F1 harness's default store URL. Must become a required env var or a Moon default; it cannot silently fall back to a deleted scheme. | OPEN — deliberately out of slice 2 (this is production code, not a test port) |

### Class A — engine constructor (25 files, 37 sites)

`Lunaris::open("memory://")` / `Lunaris::open_with_embedder("memory://", e)`
→ `open_test_engine()` / `open_test_engine_with_embedder(e)`.

| file | sites | status |
|---|---|---|
| `crates/lunaris-memory-service/src/feedback.rs` | 2 | DONE (one site uses the `open_test_storage()` seam — it wraps a bare port in a failure decorator) |
| `crates/lunaris-memory-service/src/forget.rs` | 2 | DONE |
| `crates/lunaris-memory-service/src/handover.rs` | 2 | **PIN** — asserts `queue_native == false`; Moon takes the real queue path |
| `crates/lunaris-memory-service/src/scratchpad_consolidate.rs` | 2 | **PIN** — same `queue_native` gate |
| `crates/lunaris-memory-service/src/distill.rs` | 1 | DONE |
| `crates/lunaris-memory-service/src/dream_agenda.rs` | 1 | DONE |
| `crates/lunaris-memory-service/src/protocol.rs` | 1 | DONE, except `dispatch_handover_on_memory_is_ok_and_skips` (**PIN**, same gate) |
| `crates/lunaris-memory-service/src/recall.rs` | 1 | DONE, except `as_of_time_travel_proves_bi_temporal` (**PIN**, difference #5) |
| `crates/lunaris-memory-service/src/resolve.rs` | 1 | DONE |
| `crates/lunaris-memory-service/src/scratchpad_grep.rs` | 1 | DONE |
| `crates/lunaris-memory-service/src/scratchpad_read.rs` | 1 | DONE |
| `crates/lunaris-memory-service/src/scratchpad_write.rs` | 1 | DONE |
| `crates/lunaris-memory-service/src/verify_agenda.rs` | 1 | DONE |
| `crates/lunaris-hook/src/context.rs` | 3 | DONE, except two **PIN**s: `handle_memory_scratchpad_handover_is_ok_and_skips` (`queue_native`) and `stale_memory_decays_and_banners_via_real_recall_path` (difference #6 — asserts `0.60 < score < 0.75`; the same decayed hit measures 0.011475409 on Moon) |
| `crates/lunaris-hook/src/embed_promotion.rs` | 1 | DONE |
| `crates/lunaris-hook/tests/context_inject.rs` | 4 | **RECLASSIFIED to Class C** (the URL goes to a spawned child). DONE, except `e2e_switch_on_memory_warns_and_stays_silent` (**PIN**, difference #2). `e2e_switch_on_moon_injects_additional_context` was gated on an env var CI never set and had run zero times; it now takes an ephemeral Moon and passes for real |
| `crates/lunaris-mcp/src/proxy.rs` | 1 | DONE |
| `crates/lunaris-mcp/src/tools/staging.rs` | 1 | DONE |
| `crates/lunaris-mcp/tests/cold_start.rs` | 1 | **RECLASSIFIED to Class E** (`LUNARIS_MCP_STORAGE` on a spawned child). DONE |
| `crates/lunaris/tests/optional_embedder.rs` | 3 | DONE via `open_test_store()` — difference #8 |
| `crates/lunaris/tests/embedder_dim_guardrail.rs` | 2 | DONE via `open_test_store()` |
| `crates/lunaris/tests/lazy_reranker_rss.rs` | 1 | DONE — store resolved BEFORE the RSS baseline sample |
| `crates/lunaris/tests/llamacpp_wired.rs` | 1 | DONE via `open_test_store()` — difference #8 |
| `crates/lunaris/tests/remote_llm_wired.rs` | 1 | DONE via `open_test_store()` — difference #8 |
| `crates/lunaris/tests/working_memory_roundtrip.rs` | 1 | DONE — assertions hold on both; the file's SECONDARY claim (that the internal vector-only fallback fires) now holds only under `LUNARIS_TEST_BACKEND=memory`, and the module header says so |

### Class B — storage constructor (9 files, 14 sites)

`EmbeddedStorage::connect("memory://")` → `open_test_storage()` then `.port()`.

| file | sites | status |
|---|---|---|
| `crates/lunaris-ingest/tests/raptor_wiring.rs` | 3 | DONE (`scan_deserialize` widened to `&dyn StoragePort`) |
| `crates/lunaris-ingest/tests/doctree_pipeline.rs` | 2 | DONE |
| `crates/lunaris-ingest/tests/ingest_pipeline.rs` | 1 | **PIN** — the vector half passes on Moon; the `hit.metadata["summary"]` half fails. Difference #7, a Moon-side gap |
| `crates/lunaris-hook/tests/cold_start.rs` | 2 | DONE — the p50 ≤ 50 ms / p99 ≤ 150 ms gate holds with a Moon in the loop |
| `crates/lunaris-hook/tests/idempotency.rs` | 2 | **KEEP** — asserts raw `sqlx` against `lunaris_dedupe` / `lunaris_kv` via `EmbeddedStorage::pool()`. That is the embedded SCHEMA, not a `StoragePort` contract; the harness has no seam that can serve it. 0.7.0 must re-express assertions 2-3 against `StoragePort` or delete the file (assertions 1 and 4, the LSN-equality contract, are portable) |
| `crates/lunaris-conformance/tests/run_raptor_parity.rs` | 1 | **KEEP** — `sqlite_storage()` is not a `memory://` convenience, it is one ARM of a deliberate dual-backend parity harness. Routing it through the harness would collapse both arms onto one substrate and delete the comparison. Goes with the backend. (Follow-up worth taking separately: its Moon arms are gated on `MOON_URL` and have never run; the harness could power them.) |
| `crates/lunaris-consolidate/src/dream.rs` | 1 | DONE |
| `crates/lunaris/tests/list_scopes.rs` | 1 | DONE |
| `crates/lunaris/tests/phase_14_3_reflect_prewarm.rs` | 1 | DONE via `open_test_storage_with(policy, embedder.dim())` — 4-d fixtures, difference #4 |

### Class C — subprocess env (9 files, 11 sites)

These spawn a child with `LUNARIS_STORE_URL=memory://`. The test owns a
`TestStore` for the child's lifetime and passes `store.url()`.

| file | sites | status |
|---|---|---|
| `crates/lunaris-hook/tests/emergency_drop.rs` | 2 | DONE — one store shared by all three children (warmup / timed / capture) |
| `crates/lunaris-hook/tests/malformed_envelope.rs` | 2 | DONE |
| `crates/lunaris-hook/tests/context_hybrid_recall.rs` | 1 | DONE for both degrade pins (they assert on an EMPTY store, which is backend-independent). The LIVE discriminator keeps its `LUNARIS_HOOK_TEST_MOON_URL` gate — it needs a real staged granite GGUF, which no ephemeral fixture supplies |
| `crates/lunaris-hook/tests/envelope_post_tool_use.rs` | 1 | DONE |
| `crates/lunaris-hook/tests/envelope_pre_tool_use.rs` | 1 | DONE |
| `crates/lunaris-hook/tests/envelope_session_start.rs` | 1 | DONE |
| `crates/lunaris-hook/tests/envelope_stop.rs` | 1 | DONE |
| `crates/lunaris-hook/tests/envelope_unknown.rs` | 1 | DONE |
| `crates/lunaris-hook/tests/session_switch.rs` | 1 | DONE |

### Class E — CLI argument (2 files, 2 sites)

| file | sites | status |
|---|---|---|
| `crates/lunaris-mcp/tests/server_boot.rs` | 1 | DONE — the 16-tool roster guard now boots against a real Moon |
| `crates/lunaris-mcp/tests/lazy_embedder_boot.rs` | 1 | DONE |

### Class F — doc-comment mention only (4 files, 0 sites)

No code change. Deliberately NOT edited in slice 2: the prose is accurate
*today* and becomes wrong only when 0.7.0 deletes the scheme. Editing it now
would make the tree describe a state it is not in. These belong in the deletion
commit.

| file | status |
|---|---|
| `crates/lunaris/src/handle.rs` (incl. an ```` ```ignore ```` doctest at :1000) | DEFERRED to the deletion commit |
| `crates/lunaris/tests/activation_ledger_engine.rs` | DEFERRED |
| `crates/lunaris/tests/phase_14_2_reflect_boost.rs` | DEFERRED |
| `crates/lunaris/tests/verify_agenda_engine.rs` | DEFERRED |

---

## 4. Totals after slice 2

| class | files | code sites | outcome |
|---|---:|---:|---|
| DONE (slice 1) | 3 | 0 remaining | ported |
| DELETE with the crate | 7 | 17 | untouched, by design |
| PRODUCTION decision (`er_f1.rs`) | 1 | 1 | OPEN |
| A — engine constructor | 25 | 37 | ported (7 tests pinned) |
| B — storage constructor | 9 | 14 | 7 ported, 2 KEEP |
| C — subprocess env | 9 | 11 | ported (1 test pinned) |
| E — CLI argument | 2 | 2 | ported |
| F — doc prose only | 4 | 0 | deferred to the deletion commit |
| **total** | **60** | **82** | |

**43 of the 45 portable files are ported.** The two that are not
(`lunaris-hook/tests/idempotency.rs`, `lunaris-conformance/tests/run_raptor_parity.rs`)
are recorded above with the specific reason the harness cannot serve them.

Eight individual TESTS inside otherwise-ported files are pinned to
`Policy::ForceMemory`, each with a doc comment naming the reason. Pinning
through the harness rather than leaving a raw `memory://` literal is
deliberate: when 0.7.0 removes the embedded arm, `Policy::ForceMemory`
disappears and the compiler points at exactly these eight sites.

| pinned test | reason |
|---|---|
| `memory_service::handover::handover_on_memory_backend_skips_no_queue` | `queue_native == false` |
| `memory_service::protocol::dispatch_handover_on_memory_is_ok_and_skips` | `queue_native == false` |
| `memory_service::scratchpad_consolidate::guard_queue_native_false_returns_unsupported_backend` | `queue_native == false` |
| `memory_service::recall::as_of_time_travel_proves_bi_temporal` | Moon rejects `as_of` (STORE-07) |
| `hook::context::handle_memory_scratchpad_handover_is_ok_and_skips` | `queue_native == false` |
| `hook::context::stale_memory_decays_and_banners_via_real_recall_path` | absolute score window; Moon's scale differs |
| `hook::tests::context_inject::e2e_switch_on_memory_warns_and_stays_silent` | `keyword_search` `NotSupported` |
| `ingest::tests::ingest_pipeline::community_vector_index_searchable_after_ingest` | Moon's unfiltered `vector_search` returns no metadata |

## 5. Crates with `lunaris-test-harness` in `[dev-dependencies]`

Slice 1: `lunaris`, `lunaris-memory-service`.
Slice 2 added: `lunaris-hook`, `lunaris-mcp`, `lunaris-ingest`,
`lunaris-consolidate`.

`lunaris-conformance` was NOT added — its one candidate file is a KEEP.

All of these form a dev-dependency cycle with the harness (which depends on
`lunaris`). Cargo permits that — it rejects cycles only through **normal**
dependencies. Note also that the harness SPAWNS a prebuilt `moon` binary and
depends on `lunaris-storage-moon` (the client), never the Moon server crate, so
adding it to `lunaris-mcp` does not violate the `embedded-moon`-stays-opt-in
invariant in CLAUDE.md: no test binary links Moon.

## 6. What 0.7.0 still has to do

1. Decide `crates/lunaris-bench/src/eval/er_f1.rs:98` (production default URL).
2. Re-express the eight pinned tests — six against stubbed
   `StoragePort` / `KeywordPort` doubles, one (`as_of`) against Postgres or a
   Moon that has STORE-07, one (`stale_memory_decays…`) as a ratio against an
   un-stale control hit.
3. Rewrite or delete `lunaris-hook/tests/idempotency.rs` (raw embedded SQL).
4. Delete `lunaris-conformance/tests/run_raptor_parity.rs`'s sqlite arm with the
   backend.
5. Sweep the Class F prose.
6. Separately, and NOT a test concern: close difference #7 —
   `lunaris-storage-moon::vector::vector_search` should hydrate
   `VectorHit.metadata` on the unfiltered path too, or communities BM25 content
   extraction is silently empty in production on Moon.

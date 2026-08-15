# `memory://` → Moon test port plan (0.7.0 prerequisite)

0.7.0 deletes the embedded SQLite backend — the `memory://` and `sqlite:///path`
URL schemes and the whole `lunaris-storage-embedded` crate. **60 `.rs` files
under `crates/` open storage through it today**, so the deletion is gated on
those files having somewhere else to go.

This document is the inventory and the recipe. Slice 1 (this branch) built the
seam and ported three representative files; slice 2 works the table below.

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

## 2. Behavioural differences to expect while porting

These are real, and slice 2 will hit them. They are not harness bugs.

1. **Recall against a never-written Moon errors.** `no temporal snapshot
   registered for the given AS_OF timestamp`, where the embedded backend returns
   an empty result set. Any test whose assertion is "recall finds nothing" must
   seed the store with something irrelevant first. See
   `two_fixtures_do_not_share_state` in `tests/harness_contract.rs`.
2. **`keyword_search` works on Moon and is `NotSupported` on the embedded
   backend.** Several tests assert the *degraded* vector-only fallback path
   explicitly (`working_memory_roundtrip.rs`, `context_hybrid_recall.rs`). Those
   assertions change meaning on Moon — re-read the doc comment before swapping,
   and keep the degrade pin by forcing `Policy::ForceMemory` only if the
   embedded backend still exists (it will not after 0.7.0; those pins should be
   re-expressed against a stubbed `KeywordPort` instead).
3. **`graph_native` / `queue_native` are true on Moon, false on embedded.** Tests
   that exercise entity/relation/fact writes were previously landing plain KV
   rows; on Moon they go through the real graph path. This is an *upgrade* in
   coverage but can surface latent assumptions.
4. **Vector dim is sticky on Moon.** `FT.CREATE` fixes the index width and does
   NOT resize. A fixture whose embedder is not 768-d must pass its dim through
   `open_test_engine_with_embedder` (which reads `embedder.dim()`) or
   `open_test_storage_with(policy, dim)`.

---

## 3. Inventory — all 60 files

Counts are `memory://` **code** sites (doc-comment mentions excluded).

### DONE — ported in slice 1 (3 files)

| file | shape |
|---|---|
| `crates/lunaris/tests/structured_ingest.rs` | heavy ingest + agent-supplied graph |
| `crates/lunaris/tests/forget_scoped_moon.rs` | forget / audit receipts |
| `crates/lunaris-memory-service/tests/episode_grain_loop.rs` | recall → activation ledger → boosted re-recall |

### DELETE, do not port (7 files, 17 sites)

These test the embedded backend itself, or are the backend. They go with the
crate.

| file | sites |
|---|---|
| `crates/lunaris-storage-embedded/src/lib.rs` | 4 |
| `crates/lunaris-storage-embedded/src/schema.rs` | 0 |
| `crates/lunaris-storage-embedded/tests/vector_search.rs` | 5 |
| `crates/lunaris-storage-embedded/tests/categories_membership.rs` | 3 |
| `crates/lunaris-storage-embedded/tests/concurrent_handles.rs` | 3 |
| `crates/lunaris-conformance/tests/run_storage_embedded.rs` | 1 |
| `crates/lunaris/src/open.rs` | 1 (the `"memory" \| "sqlite"` dispatch arm) |

### PRODUCTION code — a decision, not a port (1 file, 1 site)

| file | site |
|---|---|
| `crates/lunaris-bench/src/eval/er_f1.rs:98` | `.unwrap_or_else(\|_\| "memory://".to_string())` — the ER-F1 harness's default store URL. Must become a required env var or a Moon default; it cannot silently fall back to a deleted scheme. |

### Class A — trivial swap: engine constructor (25 files, 37 sites)

`Lunaris::open("memory://")` / `Lunaris::open_with_embedder("memory://", e)`
→ `open_test_engine()` / `open_test_engine_with_embedder(e)`.

Nineteen of these are `#[cfg(test)] mod tests` blocks inside `src/`, so the
change is local to the test module; the crate needs `lunaris-test-harness` added
to `[dev-dependencies]`.

| file | sites |
|---|---|
| `crates/lunaris-memory-service/src/feedback.rs` | 2 |
| `crates/lunaris-memory-service/src/forget.rs` | 2 |
| `crates/lunaris-memory-service/src/handover.rs` | 2 |
| `crates/lunaris-memory-service/src/scratchpad_consolidate.rs` | 2 |
| `crates/lunaris-memory-service/src/distill.rs` | 1 |
| `crates/lunaris-memory-service/src/dream_agenda.rs` | 1 |
| `crates/lunaris-memory-service/src/protocol.rs` | 1 |
| `crates/lunaris-memory-service/src/recall.rs` | 1 |
| `crates/lunaris-memory-service/src/resolve.rs` | 1 |
| `crates/lunaris-memory-service/src/scratchpad_grep.rs` | 1 |
| `crates/lunaris-memory-service/src/scratchpad_read.rs` | 1 |
| `crates/lunaris-memory-service/src/scratchpad_write.rs` | 1 |
| `crates/lunaris-memory-service/src/verify_agenda.rs` | 1 |
| `crates/lunaris-hook/src/context.rs` | 3 |
| `crates/lunaris-hook/src/embed_promotion.rs` | 1 |
| `crates/lunaris-hook/tests/context_inject.rs` | 4 |
| `crates/lunaris-mcp/src/proxy.rs` | 1 |
| `crates/lunaris-mcp/src/tools/staging.rs` | 1 |
| `crates/lunaris-mcp/tests/cold_start.rs` | 1 |
| `crates/lunaris/tests/optional_embedder.rs` | 3 |
| `crates/lunaris/tests/embedder_dim_guardrail.rs` | 2 |
| `crates/lunaris/tests/lazy_reranker_rss.rs` | 1 |
| `crates/lunaris/tests/llamacpp_wired.rs` | 1 |
| `crates/lunaris/tests/remote_llm_wired.rs` | 1 |
| `crates/lunaris/tests/working_memory_roundtrip.rs` | 1 (see difference #2 — re-read the doc comment) |

### Class B — storage constructor (9 files, 14 sites)

`EmbeddedStorage::connect("memory://")` → `open_test_storage()` then `.port()`.
Watch difference #4: pass the fixture's real dim if it is not 768.

| file | sites |
|---|---|
| `crates/lunaris-ingest/tests/raptor_wiring.rs` | 3 |
| `crates/lunaris-ingest/tests/doctree_pipeline.rs` | 2 |
| `crates/lunaris-ingest/tests/ingest_pipeline.rs` | 1 |
| `crates/lunaris-hook/tests/cold_start.rs` | 2 |
| `crates/lunaris-hook/tests/idempotency.rs` | 2 |
| `crates/lunaris-conformance/tests/run_raptor_parity.rs` | 1 |
| `crates/lunaris-consolidate/src/dream.rs` | 1 |
| `crates/lunaris/tests/list_scopes.rs` | 1 |
| `crates/lunaris/tests/phase_14_3_reflect_prewarm.rs` | 1 |

### Class C — subprocess env (9 files, 11 sites)

These spawn the `lunaris-hook` binary with `LUNARIS_STORE_URL=memory://`. The
test must own an `EphemeralMoon` for the duration of the child process and pass
`moon.url()` instead. All nine are in `lunaris-hook/tests/`.

| file | sites |
|---|---|
| `crates/lunaris-hook/tests/emergency_drop.rs` | 2 |
| `crates/lunaris-hook/tests/malformed_envelope.rs` | 2 |
| `crates/lunaris-hook/tests/context_hybrid_recall.rs` | 1 (see difference #2) |
| `crates/lunaris-hook/tests/envelope_post_tool_use.rs` | 1 |
| `crates/lunaris-hook/tests/envelope_pre_tool_use.rs` | 1 |
| `crates/lunaris-hook/tests/envelope_session_start.rs` | 1 |
| `crates/lunaris-hook/tests/envelope_stop.rs` | 1 |
| `crates/lunaris-hook/tests/envelope_unknown.rs` | 1 |
| `crates/lunaris-hook/tests/session_switch.rs` | 1 |

### Class E — CLI argument (2 files, 2 sites)

Same shape as C, but the URL is a positional/`--storage` argument to the
`lunaris-mcp` binary rather than an env var.

| file | sites |
|---|---|
| `crates/lunaris-mcp/tests/server_boot.rs` | 1 |
| `crates/lunaris-mcp/tests/lazy_embedder_boot.rs` | 1 |

### Class F — doc-comment mention only (4 files, 0 sites)

No code change; the prose just names a URL scheme that will not exist.

| file |
|---|
| `crates/lunaris/src/handle.rs` |
| `crates/lunaris/tests/activation_ledger_engine.rs` |
| `crates/lunaris/tests/phase_14_2_reflect_boost.rs` |
| `crates/lunaris/tests/verify_agenda_engine.rs` |

---

## 4. Totals

| class | files | code sites |
|---|---:|---:|
| DONE (slice 1) | 3 | 0 remaining |
| DELETE with the crate | 7 | 17 |
| PRODUCTION decision | 1 | 1 |
| A — engine constructor | 25 | 37 |
| B — storage constructor | 9 | 14 |
| C — subprocess env | 9 | 11 |
| E — CLI argument | 2 | 2 |
| F — doc prose only | 4 | 0 |
| **total** | **60** | **82** |

So the actual porting surface is **45 files / 64 sites** (A + B + C + E), of which
**34 are mechanical constructor swaps** (A + B) and 11 are subprocess plumbing
(C + E). Seven files are deleted rather than ported, four need only a prose
edit, and exactly one — `er_f1.rs` — is production code needing a real decision.

## 5. Crates needing `lunaris-test-harness` in `[dev-dependencies]`

Already added: `lunaris`, `lunaris-memory-service`.
Slice 2 must add: `lunaris-hook`, `lunaris-mcp`, `lunaris-ingest`,
`lunaris-consolidate`, `lunaris-conformance`.

All of these form a dev-dependency cycle with the harness (which depends on
`lunaris`). Cargo permits that — it rejects cycles only through **normal**
dependencies.

# Lunaris examples

Runnable examples. The three `quickstart-*` crates show the same
10-minute end-to-end flow (open a handle against a local Postgres backend,
ingest one episode, recall it); `multi-agent-rs` is a deeper Rust walkthrough
of the multi-agent / multi-session memory model run against a live Moon
backend.

| Example | Path | What it shows | Backend / install |
|---|---|---|---|
| Quickstart — Rust | [quickstart-rs/](quickstart-rs/) | open → ingest → scoped `recall()` (one-liner + typed DSL) | Postgres; `cargo add lunaris-memory --rename lunaris` |
| Quickstart — Python | [quickstart-py/](quickstart-py/) | ingest contract (recall is v0.3 for the binding) | Postgres; `pip install lunaris` |
| Quickstart — TypeScript | [quickstart-ts/](quickstart-ts/) | ingest contract (recall is v0.3 for the binding) | Postgres; `npm i lunaris` |
| Multi-agent / multi-session — Rust | [multi-agent-rs/](multi-agent-rs/) | hard scope isolation between two agents, multiple sessions inside one agent (via the `source` field), resume across a process boundary — all asserted against a live Moon backend | Moon (`--shards 1`); `cargo run` |

The three quickstarts reuse the **same** docker-compose Postgres image (built
from `scripts/pg-lunaris/`) so a developer can stand up one container
and exercise all three SDKs against it. `multi-agent-rs` builds its handle by
hand (`Lunaris::with_parts_keyword` + `StubEmbedder`) so it needs no external
services beyond a single-shard Moon server — see its README for the runbook.

The Rust example is canonical; the Python and TypeScript variants
mirror its shape line-for-line so the API translation is obvious.

## Surface status

- **Rust** — the canonical example. Does a real scoped `recall()` (both
  the one-liner and the typed-DSL `with_root(Vector::new(...).top(5))`
  form) and prints the hit count + top hit. The typed `Scope` +
  `EpisodeBuilder` surface is fully wired.
- **Python / TypeScript** — ingest works (dict / object wire shape;
  the typed `Scope` + `EpisodeBuilder` surface lands in v0.3 for those
  bindings). Recall is **not usable end-to-end yet**: the binding's
  `handle.recall().…​.execute()` builder has no scope parameter *and*
  no query-text parameter — the FFI bridge accepts only the default
  `Vector("chunks", k)` plan with an empty query (see
  `crates/lunaris-{py,ts}/src/dsl.rs`). Both gaps are v0.3 deliverables.
  So the Py/TS quickstarts stop at the ingest contract and point at the
  Rust example for the recall walkthrough. See each example's README
  for the precise limitation + upgrade path.

Full deliverables are tracked in `tmp/lunaris-ship-to-product-v2.md` §3.

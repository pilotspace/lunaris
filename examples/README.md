# Lunaris examples

10-minute quickstarts mirrored across the three supported language
surfaces. Each shows the same end-to-end flow: open a Lunaris handle
against a local Postgres backend, ingest one episode, then recall it.

| Language   | Path                  | Install                   |
|------------|-----------------------|---------------------------|
| Rust       | [quickstart-rs/](quickstart-rs/) | `cargo add lunaris` |
| Python     | [quickstart-py/](quickstart-py/) | `pip install lunaris` |
| TypeScript | [quickstart-ts/](quickstart-ts/) | `npm i lunaris` |

All three reuse the **same** docker-compose Postgres image (built
from `scripts/pg-lunaris/`) so a developer can stand up one container
and exercise all three SDKs against it.

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

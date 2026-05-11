# Lunaris examples

10-minute quickstarts mirrored across the three supported language
surfaces. Each shows the same end-to-end flow: open a Lunaris handle
against a local Postgres backend, ingest one episode, print the LSN.

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

## Phase 23 status

These are scaffolds, not final shipping examples:

- The recall walkthrough is a follow-up — the v0.2.x retrieval DSL
  needs one more stabilisation pass before the example shows
  `recall().vector(...).graph(...).execute()` with real hit-counting.
- The typed `Scope` + `EpisodeBuilder` SDK surface is exposed in Rust
  today; Python and TypeScript use the dict/object wire shape and
  upgrade to the typed form in v0.3.

Full Phase 23 deliverables are tracked in
`tmp/lunaris-ship-to-product-v2.md` §3.

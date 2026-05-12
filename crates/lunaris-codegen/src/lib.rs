//! lunaris-codegen — single-source-of-truth binding generation pipeline.
//!
//! Phase 8 Plan 08-01 deliverable (BIND-GEN-01 / BIND-GEN-02 / BIND-GEN-03).
//!
//! The pipeline reads a declarative [`annotations/surface.toml`](../annotations/surface.toml)
//! file describing the Rust surface (14 items — the four `Lunaris` methods
//! `open` / `ingest` / `recall` / `forget` / `snapshot`, the retrieval DSL
//! entry points `Vector::new` / `Keyword::bm25` / `Graph::anchored` and the
//! chainable `RetrievalBuilder` modifiers, plus the two pipeline-toggle
//! handles), parses it into a strongly-typed [`SurfaceIR`], then hands it to
//! two emitters:
//!
//! - [`emit_py`](mod@emit_py) — PyO3 0.26 wrapper code. Every async method is wrapped in
//!   `pyo3_async_runtimes::tokio::future_into_py` so the GIL is released
//!   across every `.await` (CLAUDE.md "never hold a lock across `.await`").
//! - [`emit_ts`](mod@emit_ts) — napi-rs 3.x glue + matching `.d.ts` declarations.
//!
//! The binary `lunaris-codegen` drives the pipeline via clap-derived flags:
//!
//! - `--emit py` / `--emit ts` — write `generated.rs` / `generated.d.ts`
//!   snapshots under `crates/lunaris-codegen/snapshots/` (Wave 2 host crates
//!   don't yet exist; snapshot-in-codegen-crate is the committed option per
//!   revision 2 — see 08-01-PLAN.md `<key_constraints>`).
//! - `--check` — regenerate into memory and diff against the committed
//!   snapshots. Exits 0 when clean, 1 with a unified diff on stderr on
//!   drift. The walker deliberately does NOT scan for `conformance.rs`
//!   modules — Plan 08-02 / 08-03 / 08-04 own those hand-written helpers.
//! - `--dump-ir` — print the extracted `SurfaceIR` as pretty JSON to stdout
//!   and exit 0. Used by the golden-snapshot commit pipeline (see
//!   `snapshots/rust_surface.json`).
//!
//! CI integrates the pipeline through the `parity-check` job in
//! `.github/workflows/ci.yml`: every PR regenerates the snapshots and fails
//! if `git diff` reports changes to the codegen-managed paths.
//!
//! ## Threat-model commitments (08-01-PLAN.md `<threat_model>`)
//!
//! - **T-08-01-03 — Elevation of Privilege (GIL across `.await`)**: the
//!   Python emitter template wraps every async body in `future_into_py`.
//!   [`tests/emitter_shape.rs`](../tests/emitter_shape.rs) grep-asserts that
//!   zero bare `.await` calls leak outside a `future_into_py` block.
//! - **T-08-01-04 — Information Disclosure (error surface)**: emitted
//!   bindings map `LunarisError` to a public-fields-only JSON shape; internal
//!   Rust error variants never leak into the `.d.ts`.
//! - **T-08-01-05 — Tampering of `conformance.rs`**: explicitly accepted
//!   (out of scope for parity-check). The `--check` walker enumerates three
//!   exact paths under `snapshots/`; anything outside that list is invisible
//!   to codegen.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]

pub mod check;
pub mod emit_py;
pub mod emit_ts;
pub mod extract;
pub mod ir;

pub use check::{CheckOutcome, check_surface, codegen_managed_paths};
pub use emit_py::emit_py;
pub use emit_ts::{TsOutput, emit_ts};
pub use extract::{SURFACE_TOML_PATH, extract_surface, extract_surface_from_str};
pub use ir::{
    IrAsync, IrKind, IrMethod, IrModule, IrParam, IrReceiver, IrReturn, IrTyRef, IrType,
    SCHEMA_VERSION, SurfaceIR,
};

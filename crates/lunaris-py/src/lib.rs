//! `lunaris-py` — PyO3 0.26 Python bindings for the Lunaris memory engine.
//!
//! Phase 8 Plan 08-02. This crate is the Python-side host of the 15-item
//! binding surface emitted by `lunaris-codegen` (Plan 08-01). The module is
//! split across:
//!
//! - [`generated`] — codegen-managed file; DO NOT EDIT. Regenerate via
//!   `cargo run -p lunaris-codegen -- --emit py` and re-copy to
//!   `crates/lunaris-py/src/generated.rs`.
//! - [`errors`] — `LunarisError` Python exception + `py_err` translator used
//!   by every generated `.map_err(py_err)` call site.
//! - [`types`] — thin helpers for Episode / ForgetRequest / Hit
//!   serialisation; used by the generated wrappers via the unqualified
//!   `pythonize::depythonize` path already emitted by Plan 08-01.
//! - [`dsl`] — Python-facing ergonomics for [`lunaris_retrieve::RetrievalBuilder`]
//!   that don't fit the code-generated surface shape (owned-self consumes).
//! - [`toggles`] — three-surface (code + env + config) wrappers for
//!   [`lunaris::GraphPipelineHandle`] and
//!   [`lunaris::ConsolidatorPipelineHandle`].
//! - [`conformance`] — feature-gated (`bindings-it`) handwritten helpers
//!   consumed by Plan 08-04. NOT codegen-managed.
//!
//! ## `#[pymodule]` ownership
//!
//! The host crate (THIS file) owns the canonical `#[pymodule] fn lunaris(...)`
//! — Plan 08-02's Rule 1 deviation on Plan 08-01 changed the emitter to
//! produce a `register_generated(py, m)` helper rather than a conflicting
//! `#[pymodule]` block, so there is exactly one `PyInit_lunaris` symbol in
//! the final cdylib.
//!
//! ## GIL discipline
//!
//! Every `.await` in [`generated`] sits inside a
//! `pyo3_async_runtimes::tokio::future_into_py` closure (CLAUDE.md mandate;
//! brace-balanced scan test in `lunaris-codegen/tests/emitter_shape.rs`).
//! The handwritten `dsl.rs`, `toggles.rs`, and `conformance.rs` follow the
//! same rule — `tests/test_gil_discipline.py` is the end-to-end proof.

#![deny(rust_2018_idioms, unreachable_pub)]
#![forbid(unsafe_code)]
#![allow(clippy::too_many_arguments)]

use pyo3::prelude::*;

mod dsl;
// Phase 21 Plan 21-01 — handwritten EmbedderConfig / RerankerConfig + the
// `lunaris_with_embedder` / `lunaris_with_reranker` free functions that the
// Python-side `dsl.open(url, embedder=..., reranker=...)` wrapper invokes.
// NOT codegen-managed; the codegen IR models the 15-item method surface,
// not builder-style construction (see embedder_config.rs module doc).
mod embedder_config;
mod errors;
mod reranker_config;
// Wave 3G — handwritten bindings for the v0.2 multi-agent partitioning surface:
// Scope, EpisodeBuilder, ScopedLunaris. NOT codegen-managed; see scope.rs for
// the rationale (lifetime constraints + `pub(crate)` into_episode visibility
// preclude codegen IR coverage).
mod scope;
mod toggles;
mod types;

// Handwritten conformance-only helpers — feature-gated so production wheels
// never ship them. NOT codegen-managed; excluded from the Plan 08-01
// parity-check walker by explicit path enumeration.
#[cfg(feature = "bindings-it")]
mod conformance;

// @generated include — Plan 08-01 emits wrapper structs + a
// `pub(crate) fn register_generated` helper. Plan 08-02's host `#[pymodule]`
// below calls that helper.
mod generated {
    // The generated file uses `py_err` as an unqualified identifier; bring
    // it into the nested module's namespace so the generated `.map_err(py_err)`
    // sites resolve.
    use super::errors::py_err;
    include!("generated.rs");
}

#[allow(unused_imports)]
pub(crate) use errors::{LunarisError, py_err};
#[allow(unused_imports)]
pub(crate) use generated::{
    // Plan 11-02b — Phase 10 conversational wrappers + 2 opaque side-types.
    PyChatAgentMemory,
    // Plan 11-02b — Phase 11 documentary wrappers.
    PyCodeRepoMemory,
    PyConsolidatorPipelineHandle,
    PyCustomerSupportHistory,
    PyDocumentKnowledgeBase,
    PyEmailThreading,
    PyGraph,
    PyGraphPipelineHandle,
    PyKeyword,
    PyLunaris,
    PyMeetingNotesMemory,
    PyMeetingNotesQuery,
    PyMultiTurnConversation,
    PyResearchPaperCorpus,
    PyRetrievalBuilder,
    PySlackArchive,
    PySlackArchiveQuery,
    PyTimelineReconstruction,
    PyVector,
};

/// Top-level `#[pymodule]` entry point — emitted by the cdylib as
/// `PyInit_lunaris`. The `[lib] name = "lunaris"` in `Cargo.toml` plus
/// `module-name = "lunaris.lunaris"` in `pyproject.toml` routes `import lunaris`
/// in Python to THIS function.
///
/// ## Plan 11-02b submodule routing
///
/// Phase 10 + 11 recipe wrappers expose as `lunaris.conversational.*` and
/// `lunaris.documentary.*`. The per-module `register_generated_{sanitised}`
/// fns emitted by Plan 11-02a's `is_legacy_single_module == false` branch
/// wire each module's classes against its own `PyModule::new_bound`
/// submodule. The root `lunaris` namespace continues to carry the Phase 8
/// `Lunaris` / `Vector` / `Keyword` / `Graph` / `RetrievalBuilder` /
/// `GraphPipelineHandle` / `ConsolidatorPipelineHandle` classes so
/// existing callers are unchanged.
#[pymodule]
fn lunaris(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Root-module codegen classes — Phase 8 surface
    // (PyLunaris / PyVector / PyKeyword / PyGraph / PyRetrievalBuilder /
    // PyGraphPipelineHandle / PyConsolidatorPipelineHandle). Plan 11-02a
    // renamed the single `register_generated` fn into per-module
    // `register_generated_{sanitised}` fns the moment a second `[[module]]`
    // block landed in `surface.toml`.
    generated::register_generated_lunaris(py, m)?;

    // Plan 11-02b — conversational submodule (Phase 10 wrappers).
    let conversational = PyModule::new(py, "conversational")?;
    generated::register_generated_lunaris_recipes_conversational(py, &conversational)?;
    m.add_submodule(&conversational)?;

    // Plan 11-02b — documentary submodule (Phase 11 wrappers).
    let documentary = PyModule::new(py, "documentary")?;
    generated::register_generated_lunaris_recipes_documentary(py, &documentary)?;
    m.add_submodule(&documentary)?;

    // Hand-written ergonomics that don't fit the codegen's single-shape
    // emitter (e.g. `open` free function, `from_env` / `from_config` toggle
    // helpers, `LunarisError` exception class).
    m.add("LunarisError", py.get_type::<LunarisError>())?;
    dsl::register(py, m)?;
    toggles::register(py, m)?;

    // Wave 3G — v0.2 multi-agent partitioning surface: Scope, EpisodeBuilder,
    // ScopedLunaris, and the `lunaris_scoped` free function. The `Lunaris`
    // class gains a `.scoped(scope)` method via the pure-Python `__init__.py`
    // lambda that calls `lunaris_scoped(self, scope)`.
    scope::register(py, m)?;

    // Phase 21 Plan 21-01 — EmbedderConfig / RerankerConfig pyclasses plus the
    // `lunaris_with_embedder` / `lunaris_with_reranker` free functions. The
    // Python-side `dsl.open(url, embedder=..., reranker=...)` wrapper threads
    // these through after `_open_handle(url)` resolves.
    embedder_config::register(py, m)?;
    reranker_config::register(py, m)?;

    #[cfg(feature = "bindings-it")]
    conformance::register(py, m)?;

    m.setattr("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

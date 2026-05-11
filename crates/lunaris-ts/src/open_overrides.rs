//! Plan 21-02 Task 3 — handwritten `#[napi] impl Lunaris` extension block
//! that adds `withEmbedder` / `withReranker` chainable methods.
//!
//! ## Approach B — napi extension (NOT a TS-side wrapper)
//!
//! Approach A (TS-side wrapper) would require swapping `package.json`'s
//! `main`/`types` to a hand-maintained module that re-delegates every
//! method of the codegen-emitted `Lunaris` class to a wrapped inner. That's
//! brittle: every new method added by codegen would need a delegation stub.
//!
//! Approach B layers fresh `#[napi]` methods onto the codegen-emitted
//! `Lunaris` class via a second `impl` block. napi-rs 3.x supports
//! multiple `#[napi] impl X` blocks per class. The codegen-emitted
//! `generated.rs` stays untouched (DO NOT EDIT contract preserved); TS
//! callers get the new methods on the same `Lunaris` object they already
//! `await`-construct from `Lunaris.open(url)`.
//!
//! ## Chainable form
//!
//! `(await Lunaris.open(url)).withEmbedder(cfg).withReranker(cfg)` —
//! fluent / chainable. Each `with*` returns a *new* `Lunaris` whose inner
//! `Arc<::lunaris::Lunaris>` carries the swapped embedder / reranker. Calls
//! with one or both swaps omitted just skip the corresponding `.with*`
//! step. This is the napi-idiomatic mirror of Python's
//! `Lunaris.open(url, embedder=..., reranker=...)` kwarg-bag — same
//! semantic outcome, different surface shape.
//!
//! The bag-form factory (`openWithConfig({ embedder, reranker })`) is
//! deliberately NOT implemented here because napi-rs `#[napi(object)]`
//! structs don't carry references to other `#[napi]` classes ergonomically
//! (no `ClassInstance<T>` field projection). The chain-form covers the
//! same use cases with one extra method call.
//!
//! The Python parallel agent may choose Approach A or B; the cross-SDK
//! parity test in Plan 21-03 keys on factory method names + opts shapes
//! (`EmbedderConfig.fastembed`, etc.), NOT on whether the `open + with_*`
//! chain is JS-side, Python-side, or Rust-side.
//!
//! ## Why a clone, not a move
//!
//! [`::lunaris::Lunaris::with_embedder`] consumes `self`. The codegen-emitted
//! `Lunaris` napi wrapper holds `Arc<::lunaris::Lunaris>` so the inner is
//! shared. We use the `Clone` impl on `::lunaris::Lunaris` to materialize an
//! owned copy, mutate, and re-wrap. The clone is cheap — the inner storage
//! port / embedder / reranker / pipelines are themselves `Arc<dyn ...>` so
//! only the bookkeeping struct is duplicated.

use std::sync::Arc;

#[allow(unused_imports)]
use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::Lunaris;
use crate::embedder_config::EmbedderConfig;
use crate::reranker_config::RerankerConfig;

#[napi]
impl Lunaris {
    /// Plan 21-02 — swap the embedder on this handle and return a new
    /// `Lunaris` carrying the override. The original handle is unaffected
    /// (its `Arc` keeps pointing at the unwrapped inner).
    #[napi(js_name = "withEmbedder")]
    pub fn with_embedder(&self, embedder: &EmbedderConfig) -> Self {
        let inner_clone: ::lunaris::Lunaris = (*self.inner).clone();
        let swapped = inner_clone.with_embedder(embedder.inner.clone());
        Self { inner: Arc::new(swapped) }
    }

    /// Plan 21-02 — swap the reranker on this handle and return a new
    /// `Lunaris` carrying the override. Symmetric to [`Self::with_embedder`].
    #[napi(js_name = "withReranker")]
    pub fn with_reranker(&self, reranker: &RerankerConfig) -> Self {
        let inner_clone: ::lunaris::Lunaris = (*self.inner).clone();
        let swapped = inner_clone.with_reranker(reranker.inner.clone());
        Self { inner: Arc::new(swapped) }
    }
}

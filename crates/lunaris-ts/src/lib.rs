//! `lunaris-ts` — napi-rs 3.x TypeScript bindings for the Lunaris memory engine.
//!
//! Phase 8 Plan 08-03. This crate is the Node-side host of the 15-item
//! binding surface emitted by `lunaris-codegen` (Plan 08-01), mirroring the
//! PyO3 host crate `lunaris-py` that Plan 08-02 landed. The module is split
//! across:
//!
//! - [`generated`] — codegen-managed file; DO NOT EDIT. Regenerate via
//!   `cargo run -p lunaris-codegen -- --emit ts` and re-copy to
//!   `crates/lunaris-ts/src/generated.rs`.
//! - [`errors`] — `napi_err` translator that every generated `.map_err(napi_err)`
//!   call site uses to lift `LunarisError` into a coarse-coded
//!   `napi::Error(status=GenericFailure, reason="CODE: message")`.
//! - [`dsl`] — TS-facing ergonomics for the retrieve DSL; camelCase
//!   aliases (`fuseRrf`, `asOf`) and the `filter(pred)` / `filterStr(s)`
//!   split that the codegen-emitted surface shape doesn't cover.
//! - [`toggles`] — three-surface (code + env + config) wrappers for
//!   [`lunaris::GraphPipelineHandle`] and
//!   [`lunaris::ConsolidatorPipelineHandle`].
//! - [`conformance`] — feature-gated (`bindings-it`) handwritten helpers
//!   consumed by Plan 08-04. NOT codegen-managed. Exposes exactly two
//!   helpers: `conformanceFixtureEpisodes` + `scanKvPrefix`. Revision 2
//!   intentionally omits `withZeroEmbedder` / `withClockSeed`.
//!
//! ## NAPI ABI pin
//!
//! Cargo.toml locks `napi = { features = ["napi8", ...] }` (Node 20 LTS
//! stable ABI). The matching runtime assertion lives in
//! `__test__/abi_pin.spec.mts` — the suite skips the full battery when
//! `process.versions.napi < 8` so a dev box on Node 18 surfaces the pin
//! mismatch with a readable reason rather than a cryptic symbol-not-found
//! error when the `.node` binary loads.
//!
//! ## Async discipline (CLAUDE.md)
//!
//! napi-rs 3.x's `tokio_rt` feature routes `#[napi] pub async fn` through
//! the shared tokio runtime. Every `.await` is awaited inside the emitted
//! method body. The CLAUDE.md invariant "never hold a `parking_lot::RwLock`
//! across `.await`" is enforced on the Rust umbrella side; the TS host
//! crate's emitted wrappers do not themselves take locks — they call into
//! `inner.ingest` / `inner.forget` / `inner.snapshot` (all owned by
//! `lunaris::Lunaris`) which already honor the invariant.

// `unreachable_pub` is INTENTIONALLY not denied here: the codegen-emitted
// `generated.rs` declares `pub struct X` / `pub async fn Y` (napi-rs 3.x's
// `#[napi]` macro requires public visibility to register the class/function
// in the emitted ctor entries). The host crate re-exports those names via
// `pub use` but the inner `mod generated` scope triggers `unreachable_pub`
// because the declarations aren't transitively reachable from `pub use` in
// the strict lint sense. Keep the rest of the `rust_2018_idioms` bundle.
#![deny(rust_2018_idioms)]
// napi-rs 3.x's `#[napi]` attribute macro expands to `#[allow(unsafe_code)]`
// around the ctor registration glue (the `Env::from_raw` / raw handle
// plumbing is inherently unsafe). The CLAUDE.md `#![forbid(unsafe_code)]`
// guidance cannot be applied at the crate root here without triggering
// "allow(unsafe_code) incompatible with previous forbid" on every `#[napi]`
// block. We use `#![deny(unsafe_code)]` instead so:
//   1. Handwritten `unsafe { .. }` in this crate stays a hard error.
//   2. The napi-rs-generated `#[allow]` scopes remain valid.
// This matches the napi-rs 3.x ecosystem convention and preserves the
// effective safety invariant that THIS crate's own code contributes zero
// unsafe blocks.
#![deny(unsafe_code)]
#![allow(clippy::too_many_arguments)]

// napi-rs 3.x's `#[napi]` attribute is re-exported by `napi_derive`. Each
// module that uses it has its own `use napi_derive::napi;` import.

mod errors;
mod dsl;
mod toggles;

// Handwritten conformance-only helpers — feature-gated so production
// `.node` binaries never ship them. NOT codegen-managed; excluded from the
// Plan 08-01 parity-check walker by explicit path enumeration.
#[cfg(feature = "bindings-it")]
mod conformance;

// @generated module — Plan 08-01 emits `#[napi] impl` blocks for
// Lunaris / Vector / Keyword / Graph / RetrievalBuilder +
// GraphPipelineHandle / ConsolidatorPipelineHandle. Exposed as a real
// `mod generated;` (not `include!`) because napi-rs 3.x's proc-macro
// state-tracking REQUIRES a real module boundary: `include!` causes
// "Did not find struct `X` parsed before expand #[napi] for impl" on
// factory methods (empirically reproduced with a minimal lib). The emitted
// file references `super::errors::napi_err` directly so no extra `use`
// is needed here.
//
// `pub use` lifts the generated classes to the crate root so the host
// registration flow (`m.add_class::<Lunaris>()?;` equivalent) and the
// handwritten `dsl.rs` / `toggles.rs` / `conformance.rs` can all refer to
// `crate::Lunaris` / `crate::RetrievalBuilder` without nested paths.
mod generated;

pub use generated::{
    ConsolidatorPipelineHandle, Graph, GraphPipelineHandle, Keyword, Lunaris,
    RetrievalBuilder, Vector,
};

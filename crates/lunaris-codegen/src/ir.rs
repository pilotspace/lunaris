//! Surface intermediate representation (SurfaceIR).
//!
//! The data structures in this module are the bridge between the declarative
//! `annotations/surface.toml` file and the two emitters (`emit_py`,
//! `emit_ts`). Everything the emitters need about a surface item — its
//! receiver shape, parameter types, async-ness, fallibility — is encoded in
//! these types.
//!
//! # Type-reference grammar (BLOCKER 11 resolution)
//!
//! Earlier revisions of the plan prototyped a custom string grammar (e.g.
//! `"Named:Lsn"`, `"Option<String>"`). Revision 1 replaced it with a
//! **structured-table** representation. Every [`IrTyRef`] variant carries a
//! `kind` discriminator; `Named` / `Option` / `Vec` nest structured fields.
//! TOML natively renders this:
//!
//! ```toml
//! ty = { kind = "named", name = "Lsn" }
//! ty = { kind = "option", inner = { kind = "str" } }
//! ty = { kind = "vec", inner = { kind = "named", name = "Hit" } }
//! ty = { kind = "handle", name = "Lunaris" }
//! ```
//!
//! Serde round-trips the shape directly — `#[serde(tag = "kind",
//! rename_all = "snake_case")]` on the enum matches the TOML. No custom
//! `TryFrom<String>` parser is needed. The
//! `tests::every_ir_ty_ref_variant_round_trips` test (in
//! `tests/surface_toml_roundtrip.rs`) locks the contract: every variant must
//! survive a `toml::to_string` → `toml::from_str` round trip unchanged.
//!
//! # Plan 11-02a additions (D1..D6)
//!
//! Three axes were added to carry the Phase 10/11 wrapper surface shapes
//! without forcing a snapshot regeneration in this plan:
//!
//! - [`IrModule::rust_crate_path`] (D2(a)) — optional per-module Rust path
//!   prefix. `None` means "use the default `::lunaris::` spelling" (existing
//!   behaviour); `Some("::lunaris_recipes::conversational")` etc. drives the
//!   emitter's method-call spelling. Annotated with `#[serde(default,
//!   skip_serializing_if = "Option::is_none")]` so the committed
//!   `rust_surface.json` snapshot stays byte-identical until a real module
//!   sets the field.
//! - [`IrTyRef::Handle`] (D3(a)) — new variant naming an opaque runtime
//!   handle (e.g. `Arc<Lunaris>`). The emitters treat it specially: no
//!   `pythonize::depythonize` / `serde_json::from_value` round-trip, just a
//!   cheap `Arc` clone lifted from the wrapper's `.inner`.
//! - [`IrMethod::clone_self`] (D6(b)) — `bool` default `false`. When `true`
//!   on a `receiver = "owned"` method the emitter replaces the stub body
//!   with `let out = self.inner.as_ref().clone().{name}(…); Ok({Target} {
//!   inner: Arc::new(out) })`. Annotated with `skip_serializing_if =
//!   "is_false"` so round-revision-1 surface.toml stays identical on disk.

use serde::{Deserialize, Serialize};

/// Surface-IR schema version. Bump when the [`SurfaceIR`] shape changes in a
/// non-additive way; the golden snapshot at
/// `crates/lunaris-codegen/snapshots/rust_surface.json` pins the current
/// value and the `surface_snapshot::golden_snapshot_matches` test enforces
/// it.
pub const SCHEMA_VERSION: u32 = 1;

/// Top-level IR emitted by [`crate::extract_surface`] and consumed by the
/// two emitters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SurfaceIR {
    pub schema_version: u32,
    /// TOML shape: `[[module]]` array-of-tables maps to this field via the
    /// rename. Callers who consume the IR through `serde_json` see the
    /// natural `modules` key; callers who consume via `toml` see `[[module]]`
    /// headers — one source of truth, two idiomatic presentations.
    #[serde(rename = "module")]
    pub modules: Vec<IrModule>,
}

/// One logical module — there is exactly one (`lunaris`) in v0.1.1; Plan
/// 11-02b adds `lunaris_recipes.conversational` and
/// `lunaris_recipes.documentary`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IrModule {
    pub name: String,
    /// Plan 11-02a D2(a): optional Rust crate-path prefix used by the
    /// emitters when spelling method-call sites. `None` is the legacy
    /// default → emitters fall back to `::lunaris::`. `Some(p)` → emitters
    /// spell calls as `{p}::{type_name}::{method}(…)`.
    ///
    /// `skip_serializing_if = "Option::is_none"` is load-bearing for
    /// `rust_surface.json` bit-stability: revision-1 `surface.toml` omits
    /// the field, so the serialised snapshot must omit it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rust_crate_path: Option<String>,
    /// Same TOML-vs-JSON duality as `SurfaceIR::modules` — `[[module.type]]`
    /// in TOML, `types` in JSON.
    #[serde(rename = "type", default)]
    pub types: Vec<IrType>,
}

/// A type exported through the bindings — `Lunaris` / `Vector` / `Keyword` /
/// `Graph` / `RetrievalBuilder` / `GraphPipelineHandle` /
/// `ConsolidatorPipelineHandle`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IrType {
    pub name: String,
    pub kind: IrKind,
    /// Source path into the Rust surface (informational — not consumed by
    /// the emitters directly, but committed as part of the golden snapshot
    /// for grep-ability).
    pub source_file: Option<String>,
    /// Doc string lifted from the surface; optional — emitters pass through
    /// when present.
    pub doc: Option<String>,
    #[serde(default, rename = "methods")]
    pub methods: Vec<IrMethod>,
}

/// Shape of a type in the binding surface. Drives the PyO3 / napi-rs class
/// attribute selection:
///
/// - `Opaque` — heap-allocated Rust struct exposed as an opaque Python /
///   TS class. Example: `Lunaris`.
/// - `Builder` — chainable struct whose `Self`-returning methods are
///   emitted as mutator methods in the target language.
/// - `Value` — plain serialisable data passed as JSON on the boundary.
///   Reserved for future use; none of the v0.1.1 14-item surface exercises
///   this variant, but the emitters accept it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IrKind {
    Opaque,
    Value,
    Builder,
}

/// A method on an [`IrType`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IrMethod {
    pub name: String,
    pub is_async: IrAsync,
    pub receiver: IrReceiver,
    #[serde(default)]
    pub params: Vec<IrParam>,
    pub returns: IrReturn,
    /// Plan 11-02a D6(b): when `true` AND `receiver = "owned"`, the
    /// emitters replace the legacy `PyNotImplementedError::new_err(...)` /
    /// `napi::Error::from_reason(...)` stub with a clone-and-rewrap body
    /// (`let out = self.inner.as_ref().clone().{name}(…); Ok({Target} {
    /// inner: Arc::new(out) })`). Default `false` + `skip_serializing_if`
    /// keeps revision-1 `surface.toml` + `rust_surface.json` byte-stable.
    #[serde(default, skip_serializing_if = "is_false")]
    pub clone_self: bool,
    pub doc: Option<String>,
}

/// Whether the method is an `async fn` at the Rust layer. Drives both
/// emitters' choice of async wrapping (PyO3 `future_into_py`; napi-rs
/// `async fn` returning `napi::Result<T>`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IrAsync {
    Yes,
    No,
}

/// Receiver shape.
///
/// - `None` — static constructor (`Lunaris::open`, `Vector::new`,
///   `Keyword::bm25`, `Graph::anchored`).
/// - `RefSelf` — `&self` method (the common case).
/// - `MutSelf` — `&mut self` (unused in v0.1.1 but accepted by the IR for
///   forward compatibility).
/// - `Owned` — `self` builder-mutator (`RetrievalBuilder::and` /
///   `.fuse_rrf` / `.top` / `.filter` / `.as_of`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IrReceiver {
    None,
    RefSelf,
    MutSelf,
    Owned,
}

/// One named argument.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IrParam {
    pub name: String,
    pub ty: IrTyRef,
}

/// Return specification. `fallible=true` means the Rust signature is
/// `Result<ty, LunarisError>`; emitters map this to `PyResult<T>` /
/// `napi::Result<T>` accordingly. `fallible=false` means an infallible
/// return (chainable DSL methods returning `Self`, or `recall` returning
/// the builder).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IrReturn {
    pub ty: IrTyRef,
    pub fallible: bool,
}

/// A reference to a Rust type — the structured-table grammar locked in
/// BLOCKER 11. `#[serde(tag = "kind")]` means TOML tables `{ kind = "...",
/// ... }` deserialise directly into the right variant.
///
/// See the crate-level module docs for the full TOML shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IrTyRef {
    /// `()` — used by methods that return nothing interesting beyond
    /// success signalling (`enable` / `disable` on the pipeline toggles).
    Unit,
    /// `&str` / `String` — passed as a Python `str` / TS `string`.
    Str,
    /// `usize` — passed as a Python `int` / TS `number`.
    Usize,
    /// `bool` — passed as a Python `bool` / TS `boolean`.
    Bool,
    /// Arbitrary JSON payload — used for `Episode` / `ForgetRequest` /
    /// `ForgetReceipt` / generic `serde_json::Value`. The binding layer
    /// serialises from / into Python `dict` / TS `object`.
    Json,
    /// `&Self` / `&mut Self` — used on static-constructor return types to
    /// indicate "this type itself" without naming it. The `Lunaris::open`
    /// entry uses `{ kind = "ref_self" }` for the `Result<Self, _>` return.
    RefSelf,
    /// Named user type — most entries use this (`Lsn`, `RetrievalBuilder`,
    /// `ForgetReceipt`, `Hit`, `EntityId`).
    Named { name: String },
    /// `Option<T>` — emitted as `Optional[T]` in Python, `T | null` in TS.
    Option { inner: Box<IrTyRef> },
    /// `Vec<T>` — emitted as `list[T]` in Python, `Array<T>` in TS.
    Vec { inner: Box<IrTyRef> },
    /// Plan 11-02a D3(a) — runtime handle passed as an opaque wrapper
    /// reference (e.g. `&PyLunaris` / `&Lunaris`). The emitter lowers this
    /// to a cheap `Arc` clone (`{name}.inner.clone()`) — NO
    /// `pythonize::depythonize` / `serde_json::from_value` round-trip.
    /// Used by every Phase 10/11 wrapper constructor that takes
    /// `Arc<Lunaris>` as its first argument.
    Handle { name: String },
}

/// `#[serde(skip_serializing_if = "is_false")]` helper. Keeps
/// `clone_self = false` from serialising into the `rust_surface.json`
/// snapshot → revision-1 IR dump stays byte-identical through Plan 11-02a.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

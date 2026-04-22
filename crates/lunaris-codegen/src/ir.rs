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
//! ```
//!
//! Serde round-trips the shape directly — `#[serde(tag = "kind",
//! rename_all = "snake_case")]` on the enum matches the TOML. No custom
//! `TryFrom<String>` parser is needed. The
//! [`tests::every_ir_ty_ref_variant_round_trips`] test (in
//! `tests/surface_toml_roundtrip.rs`) locks the contract: every variant must
//! survive a `toml::to_string` → `toml::from_str` round trip unchanged.

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

/// One logical module — there is exactly one (`lunaris`) in v0.1.1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IrModule {
    pub name: String,
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
}

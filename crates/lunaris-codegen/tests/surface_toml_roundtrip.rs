//! BLOCKER 11 guard: every [`IrTyRef`] variant MUST round-trip through TOML
//! with the structured-table grammar. This test locks the contract so a
//! future refactor that regresses `#[serde(tag = "kind")]` cannot ship.
//!
//! Plan 11-02a extends the contract with three new axes:
//!
//! - `IrTyRef::Handle { name }` (D3(a)) — `{ kind = "handle", name = "..." }`.
//! - `IrModule::rust_crate_path` (D2(a)) — optional per-module Rust prefix.
//!   The `None` case MUST serialise WITHOUT the key (`skip_serializing_if`)
//!   to preserve bit-identity of the committed `rust_surface.json`.
//! - `IrMethod::clone_self` (D6(b)) — `bool` default `false`. The `false`
//!   case MUST serialise WITHOUT the key (same rationale).

use lunaris_codegen::{
    IrAsync, IrMethod, IrModule, IrParam, IrReceiver, IrReturn, IrTyRef, extract_surface,
};
use std::path::Path;

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
struct TyRefHolder {
    ty: IrTyRef,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
struct ReturnHolder {
    returns: IrReturn,
}

#[test]
fn every_ir_ty_ref_variant_round_trips() {
    let cases = vec![
        IrTyRef::Unit,
        IrTyRef::Str,
        IrTyRef::Usize,
        IrTyRef::Bool,
        IrTyRef::Json,
        IrTyRef::RefSelf,
        IrTyRef::Named { name: "Lsn".into() },
        IrTyRef::Option { inner: Box::new(IrTyRef::Str) },
        IrTyRef::Vec { inner: Box::new(IrTyRef::Named { name: "Hit".into() }) },
        // Nested: Option<Vec<Named>>
        IrTyRef::Option {
            inner: Box::new(IrTyRef::Vec {
                inner: Box::new(IrTyRef::Named { name: "EntityId".into() }),
            }),
        },
        // Plan 11-02a D3(a) — Handle variant.
        IrTyRef::Handle { name: "Lunaris".into() },
    ];
    for original in cases {
        let wrapper = TyRefHolder { ty: original.clone() };
        let t = toml::to_string(&wrapper).expect("toml serialise");
        let parsed: TyRefHolder = toml::from_str(&t).expect("toml deserialise");
        assert_eq!(parsed.ty, original, "TOML round-trip failed for {original:?} → {t}");
    }
}

#[test]
fn ir_return_round_trips_with_fallible_flag() {
    let cases = vec![
        IrReturn { ty: IrTyRef::Named { name: "Lsn".into() }, fallible: true },
        IrReturn { ty: IrTyRef::Unit, fallible: false },
    ];
    for original in cases {
        let wrapper = ReturnHolder { returns: original.clone() };
        let t = toml::to_string(&wrapper).expect("toml serialise");
        let parsed: ReturnHolder = toml::from_str(&t).expect("toml deserialise");
        assert_eq!(parsed.returns, original);
    }
}

#[test]
fn snapshot_entry_in_surface_toml_returns_lsn() {
    // BLOCKER 1 resolution: Lunaris::snapshot returns Lsn, not SnapshotId.
    let manifest = env!("CARGO_MANIFEST_DIR");
    let ir = extract_surface(Path::new(manifest).parent().unwrap().parent().unwrap())
        .expect("extract_surface");
    let lunaris = ir
        .modules
        .iter()
        .flat_map(|m| &m.types)
        .find(|t| t.name == "Lunaris")
        .expect("Lunaris type present");
    let snap = lunaris
        .methods
        .iter()
        .find(|m| m.name == "snapshot")
        .expect("Lunaris::snapshot method present");
    match &snap.returns.ty {
        IrTyRef::Named { name } => assert_eq!(name, "Lsn"),
        other => panic!("expected Named {{ name: \"Lsn\" }}, got {other:?}"),
    }
    assert!(snap.returns.fallible, "snapshot returns Result<Lsn, _>");
}

/// Plan 11-02a D2(a) — `IrModule::rust_crate_path` is an optional field
/// that round-trips verbatim when `Some(_)` and is completely absent from
/// the serialised form when `None`. The absent-case serialisation lock is
/// load-bearing for `rust_surface.json` bit-stability until Plan 11-02b
/// annotates the existing `lunaris` module with its path.
#[test]
fn module_rust_crate_path_round_trips() {
    // Some(_) case round-trips.
    let m_with = IrModule {
        name: "lunaris_recipes.conversational".into(),
        rust_crate_path: Some("::lunaris_recipes::conversational".into()),
        types: vec![],
    };
    let t = toml::to_string(&m_with).expect("toml serialise");
    assert!(t.contains("rust_crate_path = \"::lunaris_recipes::conversational\""));
    let parsed: IrModule = toml::from_str(&t).expect("toml deserialise");
    assert_eq!(parsed, m_with);

    // None case: the `rust_crate_path` key MUST be absent from the
    // serialised TOML (`skip_serializing_if = "Option::is_none"`).
    let m_none =
        IrModule { name: "lunaris".into(), rust_crate_path: None, types: vec![] };
    let t_none = toml::to_string(&m_none).expect("toml serialise");
    assert!(
        !t_none.contains("rust_crate_path"),
        "rust_crate_path = None MUST NOT serialise — got: {t_none}"
    );
    let parsed_none: IrModule = toml::from_str(&t_none).expect("toml deserialise");
    assert_eq!(parsed_none, m_none);

    // JSON round-trip: `None` also excluded (same skip rule).
    let j_none = serde_json::to_string(&m_none).expect("json serialise");
    assert!(
        !j_none.contains("rust_crate_path"),
        "rust_crate_path = None MUST NOT serialise to JSON — got: {j_none}"
    );
}

/// Plan 11-02a D6(b) — `IrMethod::clone_self` defaults to `false` and is
/// omitted from the serialised form in that case (`skip_serializing_if =
/// "is_false"`). The `true` case is emitted + round-trips verbatim.
#[test]
fn method_clone_self_round_trips() {
    let base = IrMethod {
        name: "channel".into(),
        is_async: IrAsync::No,
        receiver: IrReceiver::Owned,
        params: vec![IrParam { name: "channel_id".into(), ty: IrTyRef::Str }],
        returns: IrReturn {
            ty: IrTyRef::Named { name: "SlackArchiveQuery".into() },
            fallible: false,
        },
        clone_self: false,
        doc: Some("Scope the archive to a single channel.".into()),
    };

    // clone_self=false must NOT appear in the serialised form.
    let t_false = toml::to_string(&base).expect("toml serialise");
    assert!(
        !t_false.contains("clone_self"),
        "clone_self = false MUST NOT serialise — got: {t_false}"
    );
    let parsed_false: IrMethod = toml::from_str(&t_false).expect("toml deserialise");
    assert_eq!(parsed_false, base);

    // Flip the flag.
    let m_true = IrMethod { clone_self: true, ..base.clone() };
    let t_true = toml::to_string(&m_true).expect("toml serialise");
    assert!(t_true.contains("clone_self = true"));
    let parsed_true: IrMethod = toml::from_str(&t_true).expect("toml deserialise");
    assert_eq!(parsed_true, m_true);

    // JSON round-trip: false omitted.
    let j_false = serde_json::to_string(&base).expect("json serialise");
    assert!(
        !j_false.contains("clone_self"),
        "clone_self = false MUST NOT serialise to JSON — got: {j_false}"
    );
}

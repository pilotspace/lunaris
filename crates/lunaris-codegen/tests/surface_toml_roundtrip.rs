//! BLOCKER 11 guard: every [`IrTyRef`] variant MUST round-trip through TOML
//! with the structured-table grammar. This test locks the contract so a
//! future refactor that regresses `#[serde(tag = "kind")]` cannot ship.

use lunaris_codegen::{IrReturn, IrTyRef, extract_surface};
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

//! Golden-snapshot lock for the extracted [`SurfaceIR`].
//!
//! Every change to `annotations/surface.toml` MUST be paired with a
//! re-run of `cargo run -p lunaris-codegen -- --dump-ir > snapshots/rust_surface.json`.
//! This test is the guard: the snapshot on disk MUST match the freshly
//! extracted IR byte-for-byte.

use lunaris_codegen::{IrAsync, IrReceiver, IrTyRef, extract_surface};
use std::path::Path;

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

#[test]
fn fifteen_items_present() {
    let ir = extract_surface(&workspace_root()).expect("extract_surface");
    let total: usize = ir.modules.iter().flat_map(|m| &m.types).map(|t| t.methods.len()).sum();
    // See extract.rs `fifteen_surface_methods_present` for the
    // plan-vs-reality arithmetic. The enumeration in the plan sums to 15;
    // we emit all 15 since each maps to a real Rust method.
    assert_eq!(total, 15, "expected 15 surface methods, got {total}");
}

#[test]
fn async_methods_are_marked() {
    let ir = extract_surface(&workspace_root()).expect("extract_surface");
    let lunaris = ir
        .modules
        .iter()
        .flat_map(|m| &m.types)
        .find(|t| t.name == "Lunaris")
        .expect("Lunaris type present");
    for n in ["open", "ingest", "forget", "snapshot"] {
        let m = lunaris
            .methods
            .iter()
            .find(|m| m.name == n)
            .unwrap_or_else(|| panic!("Lunaris::{n} missing"));
        assert_eq!(m.is_async, IrAsync::Yes, "Lunaris::{n} must be async");
    }
    let recall =
        lunaris.methods.iter().find(|m| m.name == "recall").expect("Lunaris::recall present");
    assert_eq!(recall.is_async, IrAsync::No);
    assert_eq!(recall.receiver, IrReceiver::RefSelf);
}

#[test]
fn extract_is_deterministic() {
    let a = extract_surface(&workspace_root()).expect("first extract");
    let b = extract_surface(&workspace_root()).expect("second extract");
    let ja = serde_json::to_string_pretty(&a).unwrap();
    let jb = serde_json::to_string_pretty(&b).unwrap();
    assert_eq!(ja, jb);
}

#[test]
fn golden_snapshot_matches() {
    let ir = extract_surface(&workspace_root()).expect("extract_surface");
    let got = serde_json::to_string_pretty(&ir).expect("serialise IR");
    let snapshot_path = workspace_root().join("crates/lunaris-codegen/snapshots/rust_surface.json");
    let committed = std::fs::read_to_string(&snapshot_path).unwrap_or_else(|e| {
        panic!(
            "missing golden snapshot at {}: {e}\n\
             hint: run `cargo run -p lunaris-codegen -- --dump-ir > crates/lunaris-codegen/snapshots/rust_surface.json` and commit the result",
            snapshot_path.display()
        )
    });
    assert_eq!(
        committed.trim(),
        got.trim(),
        "golden snapshot drifted from extracted IR — regenerate with --dump-ir and commit"
    );
}

#[test]
fn snapshot_method_returns_lsn() {
    let ir = extract_surface(&workspace_root()).expect("extract_surface");
    let lunaris = ir
        .modules
        .iter()
        .flat_map(|m| &m.types)
        .find(|t| t.name == "Lunaris")
        .expect("Lunaris type present");
    let snap =
        lunaris.methods.iter().find(|m| m.name == "snapshot").expect("Lunaris::snapshot present");
    match &snap.returns.ty {
        IrTyRef::Named { name } => assert_eq!(name, "Lsn"),
        other => panic!("expected Named {{ name: \"Lsn\" }}, got {other:?}"),
    }
    assert!(snap.returns.fallible);
}

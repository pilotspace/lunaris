//! INGEST-04 gate: `src/ingest.rs` must never call `atomic_write` directly.
//!
//! The single `atomic_write` per ingest path lives inside `ScopedLunaris::ingest`.
//! If this test fails, someone bypassed the invariant.

#[test]
fn ingest_handler_does_not_call_atomic_write_directly() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/ingest.rs");
    let src = std::fs::read_to_string(path).expect("src/ingest.rs must exist");

    let code_only: String = src
        .lines()
        .map(|line| {
            if let Some(idx) = line.find("//") { &line[..idx] } else { line }
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !code_only.contains("atomic_write"),
        "INGEST-04 violation: src/ingest.rs must not call atomic_write directly. \
         Use ScopedLunaris::ingest instead."
    );
}

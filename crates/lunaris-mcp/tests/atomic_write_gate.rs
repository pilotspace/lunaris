//! INGEST-04 gate: `src/tools/ingest.rs` must never call `atomic_write` directly.
//!
//! The single `atomic_write` per ingest path lives inside `ScopedLunaris::ingest`.
//! If this test fails it means someone bypassed the invariant and must fix it.

#[test]
fn record_decision_handler_does_not_call_atomic_write_directly() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/record_decision.rs");
    let src = std::fs::read_to_string(path).expect("src/tools/record_decision.rs must exist");

    // Strip comments so a comment mentioning atomic_write doesn't trip the gate.
    let code_only: String = src
        .lines()
        .map(|line| {
            // Remove everything after `//` on each line.
            if let Some(idx) = line.find("//") { &line[..idx] } else { line }
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !code_only.contains("atomic_write"),
        "INGEST-04 violation: src/tools/record_decision.rs must not call atomic_write directly. \
         Use ScopedLunaris::ingest instead."
    );
}

#[test]
fn ingest_handler_does_not_call_atomic_write_directly() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tools/ingest.rs");
    let src = std::fs::read_to_string(path).expect("src/tools/ingest.rs must exist");

    // Strip comments so a comment mentioning atomic_write doesn't trip the gate.
    let code_only: String = src
        .lines()
        .map(|line| {
            // Remove everything after `//` on each line.
            if let Some(idx) = line.find("//") { &line[..idx] } else { line }
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !code_only.contains("atomic_write"),
        "INGEST-04 violation: src/tools/ingest.rs must not call atomic_write directly. \
         Use ScopedLunaris::ingest instead."
    );
}

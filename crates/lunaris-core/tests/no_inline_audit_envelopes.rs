//! Plan 13-01 RELEASE-02 — grep-regression guard.
//!
//! Asserts NO inline `serde_json::json!({ "kind": "<Audit variant>" ... })`
//! envelopes remain in the worker crates' `src/` directories. Any match
//! indicates drift from the canonical `lunaris_core::audit::AuditEvent` +
//! `publish_audit_event` path.
//!
//! Mirrors the CI workflow step added in the same plan (Task 3); kept as a
//! cargo test so local `cargo test --workspace` catches the drift BEFORE a
//! commit is pushed.

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/crates/lunaris-core; workspace root is ../..
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // workspace root
    p
}

#[test]
fn no_inline_audit_envelopes_in_worker_crates() {
    let root = workspace_root();
    let script = r#"grep -rnE 'serde_json::json!\s*\(\s*\{\s*"kind":\s*"(Forget|VerifierArbitration|ConsolidatorPromotion|ConsolidatorArchive)"' crates/lunaris-verify/src crates/lunaris-consolidate/src 2>/dev/null || true"#;
    let out = Command::new("bash")
        .arg("-c")
        .arg(script)
        .current_dir(&root)
        .output()
        .expect("bash must be available to run the grep gate");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().is_empty(),
        "Plan 13-01 RELEASE-02: inline __lunaris_audit__ envelope detected in worker \
         crates — use lunaris_core::audit::publish_audit_event(AuditEvent::...). \
         Matches:\n{stdout}"
    );
}

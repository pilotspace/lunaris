//! Smoke tests for `scripts/bump-version.sh` (Plan 13-03 Task 1, RELEASE-04).
//!
//! The actual bump (against the real workspace) is the operator's job — these
//! tests only validate CLI argument handling:
//!   - No arg -> exit 2
//!   - Invalid semver -> exit 3
//!
//! Both validations happen BEFORE the `cargo set-version --help` tool-presence
//! check, so these tests pass without cargo-edit installed.

use std::path::PathBuf;
use std::process::Command;

/// Repo root = crates/lunaris-bench -> crates -> repo root (two parents up).
fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("crates/lunaris-bench has a parent (crates/)")
        .parent()
        .expect("crates/ has a parent (repo root)")
        .to_path_buf()
}

#[test]
fn bump_rejects_no_arg() {
    let root = repo_root();
    let output = Command::new("bash")
        .arg("scripts/bump-version.sh")
        .current_dir(&root)
        .output()
        .expect("bash is available on PATH");
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected exit 2 on missing arg, got {:?}; stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn bump_rejects_invalid_semver() {
    let root = repo_root();
    let output = Command::new("bash")
        .arg("scripts/bump-version.sh")
        .arg("not-a-semver")
        .current_dir(&root)
        .output()
        .expect("bash is available on PATH");
    assert_eq!(
        output.status.code(),
        Some(3),
        "expected exit 3 on invalid semver, got {:?}; stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
}

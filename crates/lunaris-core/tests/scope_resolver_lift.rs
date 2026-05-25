//! TDD RED: tests that `lunaris_core::scope_resolver` trait API is publicly accessible.
//! This file drives the Phase 23-01 lift. All tests MUST FAIL before the
//! implementation is moved.
//!
//! Key design: tests use `InMemoryScopeStore` (not file-backed), so:
//! - No tmpdir needed for store persistence
//! - Stability tests pass the SAME store instance for both calls
//! - No file I/O in these unit tests

use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use lunaris_core::scope_resolver::{
    InMemoryScopeStore, ScopeResolveError, ScopeStore, blake3_hex64, resolve_with,
};

fn init_git_repo(dir: &Path) {
    fn run(args: &[&str], cwd: &Path) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("git must be on PATH");
        assert!(status.success(), "git {:?} failed", args);
    }
    run(&["init", "-b", "feat/x"], dir);
    run(&["config", "user.email", "test@example.com"], dir);
    run(&["config", "user.name", "test"], dir);
    run(&["commit", "--allow-empty", "-m", "init"], dir);
    run(&["remote", "add", "origin", "https://github.com/example/repo.git"], dir);
}

#[test]
fn resolve_with_override_wins() {
    let td = TempDir::new().unwrap();
    let store = InMemoryScopeStore::new();
    let scope = resolve_with(td.path(), &store, Some("my-scope"))
        .expect("override must succeed");
    assert_eq!(scope.as_str(), "my-scope");
}

#[test]
fn resolve_with_cwd_fallback() {
    let td = TempDir::new().unwrap();
    let store = InMemoryScopeStore::new();
    let scope = resolve_with(td.path(), &store, None)
        .expect("cwd fallback must succeed");
    assert!(
        scope.as_str().starts_with("cwd_"),
        "bare dir must produce cwd_ scope, got {:?}",
        scope.as_str()
    );
}

#[test]
fn resolve_with_git_stable() {
    let td = TempDir::new().unwrap();
    // SAME store for both calls — InMemoryScopeStore persists the first derivation
    // so the second call returns the stored name, proving stability.
    let store = InMemoryScopeStore::new();
    init_git_repo(td.path());
    let s1 = resolve_with(td.path(), &store, None).expect("git scope 1");
    let s2 = resolve_with(td.path(), &store, None).expect("git scope 2");
    assert_eq!(s1, s2, "git scope must be stable");
    assert!(s1.as_str().starts_with("git_"));
}

#[test]
fn blake3_hex64_produces_64_hex_chars() {
    let h = blake3_hex64("test-input");
    assert_eq!(h.len(), 64);
    assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn invalid_override_rejected() {
    let td = TempDir::new().unwrap();
    let store = InMemoryScopeStore::new();
    assert!(
        resolve_with(td.path(), &store, Some("bad:scope")).is_err(),
        "colon in scope must fail"
    );
}

#[test]
fn in_memory_store_persists_across_calls() {
    let td = TempDir::new().unwrap();
    let store = InMemoryScopeStore::new();
    let s1 = resolve_with(td.path(), &store, None).expect("first call");
    let s2 = resolve_with(td.path(), &store, None).expect("second call");
    assert_eq!(s1, s2, "same store must return same scope on second call");
}

// This test checks that the ScopeStore trait is publicly accessible and usable
// from outside the crate. The generic bound on resolve_with means we must be
// able to name the trait.
#[test]
fn scope_store_trait_is_accessible() {
    fn _assert_trait_accessible<S: ScopeStore>(_s: &S) {}
    let store = InMemoryScopeStore::new();
    _assert_trait_accessible(&store);
}

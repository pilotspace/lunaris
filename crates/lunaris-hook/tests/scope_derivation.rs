//! Scope derivation test: same cwd+git repo → same scope name as produced by
//! lunaris_core::scope_resolver::resolve_with (via InMemoryScopeStore).
//!
//! Verifies that lunaris-hook and lunaris-core produce bit-identical scopes for
//! the same repo (the shared lunaris-core implementation guarantees this, but
//! we pin it with an integration test so a future refactor can't regress it).

use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use lunaris_core::scope_resolver::{InMemoryScopeStore, resolve_with};

fn init_git_repo(dir: &Path) {
    fn run(args: &[&str], cwd: &Path) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("git must be on PATH");
        assert!(status.success(), "git {:?} failed", args);
    }
    run(&["init", "-b", "main"], dir);
    run(&["config", "user.email", "test@example.com"], dir);
    run(&["config", "user.name", "test"], dir);
    run(&["commit", "--allow-empty", "-m", "init"], dir);
    run(&["remote", "add", "origin", "https://github.com/example/hook-repo.git"], dir);
}

/// lunaris-hook's scope resolver produces the same scope as calling
/// lunaris_core::scope_resolver::resolve_with directly with the same args.
#[test]
fn hook_scope_matches_core_scope_resolver() {
    let td = TempDir::new().unwrap();
    let scopes_path = td.path().join("scopes.json");
    init_git_repo(td.path());

    // Call via lunaris-hook's wrapper (delegates to lunaris_core::scope_resolver)
    let hook_scope = lunaris_hook::scope::resolve_with_path(
        td.path(),
        &scopes_path,
        None,
    )
    .expect("hook scope must resolve");

    // Call directly via lunaris-core with InMemoryScopeStore.
    // Note: this uses an InMemoryScopeStore (no file I/O) — the scope name
    // itself is deterministic from cwd+git, so both calls produce the same
    // name even with different store backends.
    let mem_store = InMemoryScopeStore::new();
    let core_scope = resolve_with(td.path(), &mem_store, None)
        .expect("core scope must resolve");

    assert_eq!(
        hook_scope,
        core_scope,
        "hook scope {:?} must match core scope {:?} for the same repo",
        hook_scope.as_str(),
        core_scope.as_str()
    );
    assert!(hook_scope.as_str().starts_with("git_"));
}

/// LUNARIS_HOOK_SCOPE env override produces the specified scope string.
#[test]
fn hook_scope_env_override_wins() {
    let td = TempDir::new().unwrap();
    let scopes_path = td.path().join("scopes.json");

    let scope = lunaris_hook::scope::resolve_with_path(
        td.path(),
        &scopes_path,
        Some("my-project"),
    )
    .expect("override must succeed");

    assert_eq!(scope.as_str(), "my-project");
}

/// Two calls with the same scopes file return the same scope (file persistence works).
#[test]
fn hook_scope_is_stable_across_calls() {
    let td = TempDir::new().unwrap();
    let scopes_path = td.path().join("scopes.json");

    let s1 = lunaris_hook::scope::resolve_with_path(td.path(), &scopes_path, None)
        .expect("first call");
    let s2 = lunaris_hook::scope::resolve_with_path(td.path(), &scopes_path, None)
        .expect("second call");

    assert_eq!(s1, s2, "scope must be stable across calls");
}

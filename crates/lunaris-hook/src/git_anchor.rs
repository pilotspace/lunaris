//! Git-anchoring (engram-soul-loop task 5): resolves the current repo HEAD
//! for a capture's cwd so every episode can carry a verifiable code anchor
//! (`meta.git_head`). Fail-open everywhere: any git failure yields `None`
//! and never delays a capture beyond the 300ms subprocess cap.
//!
//! RED-phase placeholder (`.add/tasks/git-anchoring/TASK.md` §4 TESTS): the
//! production `head_for_cwd` / cache / `ttl_cache_len` land in the GREEN
//! commit. Until then this module has no non-test items, so `cargo build`
//! still succeeds (an empty module) but `cargo test -p lunaris-hook` fails to
//! compile this file's test suite — the sanctioned "missing-module compile
//! error" RED evidence for a brand-new module (contract freeze note).

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh temp git repo with one empty commit, so `git rev-parse HEAD`
    /// resolves deterministically.
    fn init_temp_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .expect("git init must run — this box has git installed per §0 GROUND");
        assert!(status.success(), "git init failed");
        let status = std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-m",
                "x",
            ])
            .current_dir(dir.path())
            .status()
            .expect("git commit must run");
        assert!(status.success(), "git commit --allow-empty failed");
        dir
    }

    fn git_head_via_cli(cwd: &std::path::Path) -> String {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(cwd)
            .output()
            .expect("git rev-parse HEAD must run");
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// `head_for_cwd` resolves the real 40-hex HEAD of a temp repo, matching
    /// what the `git` CLI itself reports.
    #[tokio::test]
    async fn resolves_head_in_temp_repo() {
        let repo = init_temp_repo();
        let expected = git_head_via_cli(repo.path());

        let resolved = head_for_cwd(repo.path()).await.expect("HEAD must resolve in a real repo");

        assert_eq!(resolved, expected);
        assert_eq!(resolved.len(), 40, "HEAD must be a 40-hex sha, got {resolved}");
        assert!(resolved.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// A plain (non-repo) cwd must resolve to `None` — no repo, no error.
    #[tokio::test]
    async fn none_outside_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolved = head_for_cwd(dir.path()).await;
        assert!(resolved.is_none(), "a plain (non-repo) cwd must resolve to None, got {resolved:?}");
    }

    /// A second `head_for_cwd` call for the SAME cwd inside the 5s TTL must
    /// reuse the cached value rather than invoking `git` again. Uses a
    /// per-key invocation counter (test-only) rather than the process-wide
    /// `ttl_cache_len()` count, so the assertion is immune to other tests
    /// concurrently populating the shared cache under their OWN (distinct
    /// tempdir) keys.
    #[tokio::test]
    async fn cache_hits_within_ttl_skip_second_subprocess() {
        let repo = init_temp_repo();
        let canon = std::fs::canonicalize(repo.path()).expect("canonicalize");

        let first = head_for_cwd(repo.path()).await;
        let count_after_first = resolve_calls_for_test(&canon);
        assert_eq!(count_after_first, 1, "first call must invoke the git subprocess exactly once");

        let second = head_for_cwd(repo.path()).await;
        let count_after_second = resolve_calls_for_test(&canon);

        assert_eq!(first, second, "cached value must match the fresh resolution");
        assert_eq!(
            count_after_second, count_after_first,
            "a call within the 5s TTL must reuse the cached value, not invoke git again"
        );
        assert!(ttl_cache_len() >= 1, "the cache must hold at least our own entry");
    }
}

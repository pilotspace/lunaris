//! Git-anchoring (engram-soul-loop task 5): resolves the current repo HEAD
//! for a capture's cwd so every episode can carry a verifiable code anchor
//! (`meta.git_head`). Fail-open everywhere: any git failure yields `None`
//! and never delays a capture beyond the 300ms subprocess cap.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Cache entry TTL (`.add/tasks/git-anchoring/TASK.md` §3 CONTRACT). Freeze
/// note: this is a guess; correctness is unaffected — HEAD staleness within
/// this window is indistinguishable from a capture-time race anyway.
const TTL: Duration = Duration::from_secs(5);
/// Opportunistic prune threshold — bounds unbounded per-cwd growth without a
/// background sweeper (§1 Reject: "unbounded cache growth -> forbidden").
const CAP: usize = 64;
/// Hard subprocess cap: a hung/slow `git` must never delay a capture beyond
/// this (§1 Reject: a git failure must never surface as a capture error, and
/// must never block beyond this bound).
const SUBPROCESS_TIMEOUT: Duration = Duration::from_millis(300);

/// `(seen_at, resolved_head)` — the contract's frozen cache value shape.
type CacheEntry = (Instant, Option<String>);

static CACHE: LazyLock<Mutex<HashMap<PathBuf, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Resolve `git rev-parse HEAD` for `cwd`, TTL-cached per canonicalized path.
/// Returns `None` on ANY failure: no repo, no `git` binary, non-zero exit,
/// non-hex output, or timeout — never an `Err`, so callers can stamp
/// `meta.git_head` without ever failing the capture.
///
/// Lock discipline (UNSAFE_POLICY / workspace convention): the cache lock is
/// NEVER held across the subprocess `.await`. The fast path checks and drops
/// the guard before any `.await`; the slow path resolves with no lock held,
/// then re-locks only to insert.
pub(crate) async fn head_for_cwd(cwd: &Path) -> Option<String> {
    let key = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());

    {
        let cache = CACHE.lock();
        if let Some((seen, value)) = cache.get(&key)
            && seen.elapsed() < TTL
        {
            return value.clone();
        }
        // guard dropped here — never held across the subprocess await below
    }

    let resolved = resolve_head(&key).await;

    let mut cache = CACHE.lock();
    if cache.len() > CAP {
        let now = Instant::now();
        cache.retain(|_, (seen, _)| now.duration_since(*seen) < TTL);
    }
    cache.insert(key, (Instant::now(), resolved.clone()));
    resolved
}

/// Spawn `git rev-parse HEAD` in `cwd` under a hard 300ms cap. Any failure
/// mode (spawn error, timeout, non-zero exit, non-utf8, non-40-hex output)
/// collapses to `None` — this fn never panics and never propagates an error.
async fn resolve_head(cwd: &Path) -> Option<String> {
    #[cfg(test)]
    record_resolve_call_for_test(cwd);

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("rev-parse")
        .arg("HEAD")
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let output = tokio::time::timeout(SUBPROCESS_TIMEOUT, cmd.output()).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    let is_40_hex = trimmed.len() == 40 && trimmed.bytes().all(|b| b.is_ascii_hexdigit());
    if is_40_hex { Some(trimmed.to_owned()) } else { None }
}

/// Test seam (`.add/tasks/git-anchoring/TASK.md` §3 CONTRACT): current cache
/// size, observable for TTL/prune assertions.
#[cfg(test)]
pub(crate) fn ttl_cache_len() -> usize {
    CACHE.lock().len()
}

/// Test-only per-key subprocess invocation counter — deterministic proof
/// that a within-TTL `head_for_cwd` call skips `resolve_head` entirely,
/// immune to unrelated tests hammering the SAME process-wide `CACHE` static
/// concurrently on their OWN (distinct tempdir) keys.
#[cfg(test)]
static RESOLVE_CALLS: LazyLock<Mutex<HashMap<PathBuf, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
fn record_resolve_call_for_test(cwd: &Path) {
    *RESOLVE_CALLS.lock().entry(cwd.to_path_buf()).or_insert(0) += 1;
}

#[cfg(test)]
pub(crate) fn resolve_calls_for_test(cwd: &Path) -> usize {
    RESOLVE_CALLS.lock().get(cwd).copied().unwrap_or(0)
}

// ── engram-soul-loop task 6 (staleness-pass) — changed-files diff ─────────
//
// `.add/tasks/staleness-pass/TASK.md` §3 CONTRACT: `git diff --name-only
// <anchor>..HEAD`, same fail-open/timeout/caching pattern as
// `head_for_cwd` above, keyed by `(canonical cwd, anchor_head)`.

/// Hard cap on distinct anchor diffs resolved per sweep call site (§1 Must
/// / Reject: "at most 8 DISTINCT anchor diffs resolved per sweep call
/// site — excess anchors skipped fail-open, caught next sweep").
pub(crate) const MAX_ANCHOR_DIFFS_PER_SWEEP: usize = 8;

/// `(seen_at, resolved_changed_files)` — mirrors `CacheEntry` above.
type DiffCacheEntry = (Instant, Option<HashSet<String>>);

static DIFF_CACHE: LazyLock<Mutex<HashMap<(PathBuf, String), DiffCacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Resolve `git diff --name-only <anchor>..HEAD` for `cwd`, TTL-cached per
/// canonicalized `(cwd, anchor)` pair. Returns `None` on ANY failure: no
/// repo, no `git` binary, non-zero exit, non-utf8 output, or timeout —
/// never an `Err`, so callers (staleness assessment, the verify-agenda
/// sweep) can fail open to "not stale" rather than ever surfacing a git
/// failure.
///
/// Lock discipline: identical to [`head_for_cwd`] — the cache lock is
/// never held across the subprocess `.await`.
pub(crate) async fn changed_files_since(cwd: &Path, anchor: &str) -> Option<HashSet<String>> {
    let canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let key = (canon, anchor.to_owned());

    {
        let cache = DIFF_CACHE.lock();
        if let Some((seen, value)) = cache.get(&key)
            && seen.elapsed() < TTL
        {
            return value.clone();
        }
        // guard dropped here — never held across the subprocess await below
    }

    let resolved = resolve_diff(&key.0, anchor).await;

    let mut cache = DIFF_CACHE.lock();
    if cache.len() > CAP {
        let now = Instant::now();
        cache.retain(|_, (seen, _)| now.duration_since(*seen) < TTL);
    }
    cache.insert(key, (Instant::now(), resolved.clone()));
    resolved
}

/// Spawn `git diff --name-only <anchor>..HEAD` in `cwd` under the same hard
/// 300ms cap as [`resolve_head`]. Any failure mode collapses to `None`.
async fn resolve_diff(cwd: &Path, anchor: &str) -> Option<HashSet<String>> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("diff")
        .arg("--name-only")
        .arg(format!("{anchor}..HEAD"))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    let output = tokio::time::timeout(SUBPROCESS_TIMEOUT, cmd.output()).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    Some(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect())
}

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
        assert!(
            resolved.is_none(),
            "a plain (non-repo) cwd must resolve to None, got {resolved:?}"
        );
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

    // ── engram-soul-loop task 6 (staleness-pass) — changed_files_since ────
    // `.add/tasks/staleness-pass/TASK.md` §3 CONTRACT: `git diff --name-only
    // <anchor>..HEAD`, 300ms cap, fail-open `None`, TTL-cached per (canonical
    // cwd, anchor_head). Mirrors `head_for_cwd`'s test style above.

    fn write_file(dir: &std::path::Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).expect("write must succeed");
    }

    fn commit_all(dir: &std::path::Path, msg: &str) {
        let status = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir)
            .status()
            .expect("git add must run");
        assert!(status.success(), "git add failed");
        let status = std::process::Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-m", msg])
            .current_dir(dir)
            .status()
            .expect("git commit must run");
        assert!(status.success(), "git commit failed");
    }

    /// `changed_files_since(cwd, anchor)` must report a file touched between
    /// `anchor` and the repo's current HEAD.
    #[tokio::test]
    async fn changed_files_since_reports_touched_file() {
        let repo = init_temp_repo();
        write_file(repo.path(), "a.txt", "one");
        commit_all(repo.path(), "seed a.txt");
        let anchor = git_head_via_cli(repo.path());

        write_file(repo.path(), "a.txt", "two");
        commit_all(repo.path(), "touch a.txt");

        let changed = changed_files_since(repo.path(), &anchor)
            .await
            .expect("a real repo with a real diff must resolve Some");
        assert!(changed.contains("a.txt"), "changed set must contain a.txt, got {changed:?}");
    }

    /// A plain (non-repo) cwd must resolve to `None` — fail-open, never an
    /// `Err`.
    #[tokio::test]
    async fn changed_files_since_none_outside_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolved = changed_files_since(dir.path(), &"a".repeat(40)).await;
        assert!(resolved.is_none(), "a non-repo cwd must resolve to None, got {resolved:?}");
    }

    /// A second call for the SAME (cwd, anchor) pair inside the TTL must
    /// reuse the cached value rather than invoking `git` again.
    #[tokio::test]
    async fn changed_files_since_cached_within_ttl() {
        let repo = init_temp_repo();
        write_file(repo.path(), "a.txt", "one");
        commit_all(repo.path(), "seed a.txt");
        let anchor = git_head_via_cli(repo.path());
        write_file(repo.path(), "a.txt", "two");
        commit_all(repo.path(), "touch a.txt");

        let first = changed_files_since(repo.path(), &anchor).await;
        let second = changed_files_since(repo.path(), &anchor).await;
        assert_eq!(first, second, "cached value must match the fresh resolution");
    }

    /// `MAX_ANCHOR_DIFFS_PER_SWEEP` is the frozen §3 CONTRACT bound on
    /// distinct anchor diffs resolved per sweep call site.
    #[test]
    fn max_anchor_diffs_per_sweep_is_eight() {
        assert_eq!(MAX_ANCHOR_DIFFS_PER_SWEEP, 8);
    }
}

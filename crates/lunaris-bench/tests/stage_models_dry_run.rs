//! N-04 D5 — `stage-models` dry-run contract.
//!
//! Builds the `stage-models` binary, invokes it with
//! `--dry-run --cache-dir /tmp/lunaris-stage-test-<rand>`, and verifies:
//!
//! 1. The binary exits 0.
//! 2. The cache dir was NOT created (a dry run touches no filesystem).
//! 3. Stdout names both model repositories
//!    (`ibm-granite/granite-embedding-r2` +
//!     `BAAI/bge-reranker-v2-m3`) and the canonical GGUF SHA-256s
//!     listed in the N-04 brief.
//!
//! The test does NOT exercise the live HF Hub download path — that
//! requires network egress and is covered by the staged-artifact
//! manual-UAT recipe in docs/runbooks/v0.4-soak.md.

use std::path::PathBuf;
use std::process::Command;

const GRANITE_REPO: &str = "ibm-granite/granite-embedding-r2";
const BGE_REPO: &str = "BAAI/bge-reranker-v2-m3";
const GRANITE_GGUF_SHA: &str = "0768a38b0bc9900e89bb15ae0b6ea2ca7db130759e0eca226119610aedf5e276";
const BGE_GGUF_SHA: &str = "6cdcc566200dba69553a89a9d59ff6d631e33969bc9367eff6914919f7722a1c";

fn stage_models_binary() -> PathBuf {
    // Resolve `target/{profile}/stage-models` via env!("CARGO_BIN_EXE_<name>")
    // — Cargo guarantees the bin is built before the integration test runs.
    PathBuf::from(env!("CARGO_BIN_EXE_stage-models"))
}

fn tmp_cache_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("lunaris-stage-test-{nanos}"))
}

#[test]
fn dry_run_exits_zero_and_creates_nothing() {
    let bin = stage_models_binary();
    let cache = tmp_cache_dir();
    assert!(!cache.exists(), "tmp cache dir must not pre-exist: {}", cache.display());

    let out = Command::new(&bin)
        .args(["--dry-run", "--cache-dir"])
        .arg(&cache)
        .output()
        .expect("spawn stage-models");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    eprintln!("--- stage-models stdout ---\n{stdout}");
    eprintln!("--- stage-models stderr ---\n{stderr}");

    assert!(out.status.success(), "stage-models --dry-run must exit 0, got {:?}", out.status);
    assert!(!cache.exists(), "--dry-run must NOT create the cache dir at {}", cache.display());
    assert!(
        stdout.contains(GRANITE_REPO),
        "stdout must name granite repo {GRANITE_REPO}; got:\n{stdout}"
    );
    assert!(stdout.contains(BGE_REPO), "stdout must name bge repo {BGE_REPO}; got:\n{stdout}");
    assert!(
        stdout.contains(GRANITE_GGUF_SHA),
        "stdout must mention granite GGUF SHA {GRANITE_GGUF_SHA}; got:\n{stdout}"
    );
    assert!(
        stdout.contains(BGE_GGUF_SHA),
        "stdout must mention bge GGUF SHA {BGE_GGUF_SHA}; got:\n{stdout}"
    );
}

#[test]
fn help_lists_both_repos_and_ggufs() {
    let bin = stage_models_binary();
    let out = Command::new(&bin).arg("--help").output().expect("spawn stage-models --help");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "--help must exit 0");
    assert!(stdout.contains(GRANITE_REPO), "--help missing granite repo");
    assert!(stdout.contains(BGE_REPO), "--help missing bge repo");
    assert!(stdout.contains(GRANITE_GGUF_SHA), "--help missing granite GGUF SHA");
    assert!(stdout.contains(BGE_GGUF_SHA), "--help missing bge GGUF SHA");
}

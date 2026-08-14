//! Safety + hygiene ratchet for the version-controlled LongMemEval harness
//! (`scripts/bench/lme/`), landed in 0.6.2 task #12.
//!
//! Every measured LME question starts by `FLUSHALL`-ing its target Moon,
//! because LongMemEval-S needs each question to retrieve from its own haystack
//! and Moon's `StartsWith` source filter fails open. That makes every runner a
//! loaded gun: pointed at port 6381 — the operator's live personal memory
//! store, launchd unit `dev.lunaris.moon-6381` — it destroys hundreds of
//! thousands of keys irrecoverably.
//!
//! The pre-0.6.2 `tmp/` working copies proved the guard cannot be left to
//! per-script discipline: `tmp/validate_ollama/run_lme.sh` had the refusal,
//! `tmp/validate_ollama/fill_cache.sh` did not, and
//! `tmp/llamacpp_gate/run_gate50.sh` FLUSHALLed 6381 on purpose. So the guard
//! lives in exactly one place (`lib.sh`) and this test pins that **every**
//! entry point routes through it.
//!
//! It also pins the sanitation that made the harness committable at all: no
//! personal absolute paths, no in-repo key files, no credential literals.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/crates/lunaris-bench.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // workspace root
    p
}

fn harness_dir() -> PathBuf {
    workspace_root().join("scripts/bench/lme")
}

/// Every `*.sh` in the harness directory except the sourced library itself.
fn entry_points() -> Vec<PathBuf> {
    let dir = harness_dir();
    let mut out: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sh"))
        .filter(|p| p.file_name().is_some_and(|n| n != "lib.sh"))
        .collect();
    out.sort();
    out
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The harness must exist in the repo at all — the whole point of task #12 is
/// that it stopped living only in a gitignored `tmp/`.
#[test]
fn harness_is_version_controlled() {
    let dir = harness_dir();
    assert!(
        dir.is_dir(),
        "{} is missing: the LME harness must live in the repo, not in a \
         gitignored tmp/ working copy",
        dir.display()
    );
    for required in [
        "lib.sh",
        "run_lme.sh",
        "fill_cache.sh",
        "ab_run.sh",
        "chain_fill_then_ab.sh",
        "moon_watchdog.sh",
        "tally.py",
        "README.md",
        "questions/offsets125.tsv",
    ] {
        assert!(dir.join(required).is_file(), "scripts/bench/lme/{required} is missing");
    }
    assert!(!entry_points().is_empty(), "no shell entry points found under {}", dir.display());
}

/// The single refusal, in the single place.
#[test]
fn lib_hard_refuses_the_live_store_port() {
    let lib = read(&harness_dir().join("lib.sh"));
    assert!(lib.contains("lme_guard_port"), "lib.sh must define lme_guard_port");
    assert!(
        lib.contains("6381"),
        "lib.sh must name port 6381 explicitly in its reserved-port list — a \
         guard that does not know the live store's port guards nothing"
    );
    assert!(
        lib.contains("LME_RESERVED_PORTS"),
        "lib.sh must expose LME_RESERVED_PORTS so operators can reserve more \
         ports without editing the guard"
    );
}

/// No entry point may reach a Moon without passing through the guard.
#[test]
fn every_entry_point_sources_lib_and_guards_its_port() {
    for path in entry_points() {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let body = read(&path);
        assert!(
            body.contains("lib.sh"),
            "{name}: must source scripts/bench/lme/lib.sh (it carries the \
             reserved-port refusal)"
        );
        assert!(
            body.contains("lme_guard_port"),
            "{name}: must call lme_guard_port on its target port BEFORE any \
             Moon is pinged, started or flushed. 6381 is the live store and a \
             measured run FLUSHALLs its target."
        );
    }
}

/// Nothing destructive may bypass the guarded helpers.
#[test]
fn no_entry_point_flushes_a_moon_directly() {
    for path in entry_points() {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for (n, line) in read(&path).lines().enumerate() {
            let code = line.split('#').next().unwrap_or("");
            assert!(
                !code.to_lowercase().contains("flushall"),
                "{name}:{}: raw FLUSHALL. Use lme_moon_flush, which guards the \
                 port first.",
                n + 1
            );
        }
    }
}

/// Reproducibility: a hardcoded checkout path makes a fresh clone unrunnable.
#[test]
fn harness_contains_no_personal_absolute_paths() {
    let dir = harness_dir();
    let mut files = entry_points();
    files.push(dir.join("lib.sh"));
    files.push(dir.join("tally.py"));
    files.push(dir.join("README.md"));

    // `/Users/` and `/Volumes/` appear in no portable default; `$HOME` is the
    // supported way to reach an operator's home directory.
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for (n, line) in read(&path).lines().enumerate() {
            for needle in ["/Users/", "/Volumes/", "/home/"] {
                assert!(
                    !line.contains(needle),
                    "{name}:{}: hardcoded personal path ({needle}). Derive from \
                     REPO_ROOT or $HOME so a fresh clone works.",
                    n + 1
                );
            }
        }
    }
}

/// Credentials come from the environment, never from the tree.
#[test]
fn harness_contains_no_credentials_or_in_repo_key_files() {
    let dir = harness_dir();
    let mut files = entry_points();
    files.push(dir.join("lib.sh"));
    files.push(dir.join("tally.py"));

    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for (n, line) in read(&path).lines().enumerate() {
            // The pre-port scripts did `MINIMAX_API_KEY="$(cat tmp/minimax.key)"`
            // — a repo-relative secret path. Keys now arrive via the
            // environment or LUNARIS_BENCH_KEY_FILE.
            assert!(
                !line.contains(".key)") && !line.contains("tmp/minimax"),
                "{name}:{}: reads a key file from inside the repo. Use \
                 MINIMAX_API_KEY or LUNARIS_BENCH_KEY_FILE.",
                n + 1
            );
            // Literal credential shapes: JWT (MiniMax), OpenAI-style, and
            // GitHub tokens.
            for needle in ["eyJhbGciOi", "sk-proj-", "sk-ant-", "ghp_"] {
                assert!(
                    !line.contains(needle),
                    "{name}:{}: looks like a committed credential ({needle}...)",
                    n + 1
                );
            }
        }
    }
}

/// The manifest defines what "N=125" means. A silent edit invalidates every
/// comparison against the recorded baselines.
#[test]
fn n125_manifest_has_125_questions() {
    let body = read(&harness_dir().join("questions/offsets125.tsv"));
    let rows: Vec<&str> =
        body.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    assert_eq!(rows.len(), 125, "questions/offsets125.tsv must hold exactly 125 questions");
    for (n, row) in rows.iter().enumerate() {
        let mut cols = row.split_whitespace();
        let offset = cols.next().unwrap_or("");
        assert!(
            offset.parse::<u32>().is_ok(),
            "offsets125.tsv row {}: column 1 must be a numeric offset, got {offset:?}",
            n + 1
        );
        assert!(
            cols.next().is_some(),
            "offsets125.tsv row {}: column 2 (category) is missing",
            n + 1
        );
    }
}

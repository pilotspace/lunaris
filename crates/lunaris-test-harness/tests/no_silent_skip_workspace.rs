//! Workspace backstop: a test that skips must say so through a helper that can
//! REFUSE to skip.
//!
//! ## Why this is not a fourth copy of an existing guard
//!
//! Two crates already police their own silent skips —
//! `lunaris-storage-moon/tests/no_silent_moon_skip.rs` and
//! `lunaris-conformance/tests/no_silent_skip.rs`. Both were keyed on the
//! wording of the instances that existed when they were written, and both went
//! blind to the next family that appeared:
//!
//! * storage-moon's sweep required the phrase `"not reachable"`, which matches
//!   the `connect_or_skip` family and nothing else. Six files reaching Moon via
//!   `EphemeralMoon::spawn()` say `no ephemeral Moon (..); SKIP` instead, so it
//!   saw none of them — including the F21 and F22 guards, both running in the
//!   strict integration job.
//! * conformance's sweep required `eprintln!("SKIP` with `SKIP` immediately
//!   after the quote, while every real site there writes
//!   `eprintln!("run_storage_moon: SKIP ..)`. It has never matched anything.
//!
//! A third crate-local copy would have inherited the same failure mode. This
//! guard is workspace-wide and keyed on the DECISION's observable form — a
//! print announcing a skip — so it does not care which constructor, phrase or
//! crate the next one uses.
//!
//! ## The rule, and why it is per-SITE
//!
//! Any code line under `crates/*/tests/` that prints and mentions `SKIP` is an
//! unrouted skip, unless the file is listed below with a reason.
//!
//! Per-line, not per-file, on purpose. Both existing guards ask whether a file
//! MENTIONS its routing helper anywhere — which a file with one routed site and
//! two unrouted ones satisfies. `run_storage_moon.rs` is exactly that shape
//! today: on conformance's roster, routing one skip, printing two more itself.
//! A routed call site does not print at all — the helper does — so "prints a
//! skip" is a sound per-site test for being unrouted.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Skips that are NOT about a missing backend and must never route through a
/// strict helper: the strict integration job deliberately has no GGUF, no
/// Ollama, no cloud API key and no bench corpus, so failing there would be
/// wrong. Each entry carries the gate it honours.
const ENVIRONMENT_GATED: &[(&str, &str)] = &[
    (
        "lunaris-bench/tests/budget_assertions.rs",
        "prints a per-suite summary line, not a skip decision",
    ),
    (
        "lunaris-extract/tests/extractor_contract.rs",
        "OLLAMA_URL / cloud API key — no inference in CI",
    ),
    ("lunaris/tests/lazy_reranker_rss.rs", "LUNARIS_RERANKER_GGUF + platform RSS sampling"),
    (
        "lunaris-storage-moon/tests/multishard_live.rs",
        "LUNARIS_TEST_MOON_SHARDS, and a server-version floor",
    ),
];

/// Moon-dependent skips that still print for themselves, in crates the strict
/// integration job does NOT run. Real debt, tracked rather than hidden: a
/// silent skip is only harmless while nothing promises a Moon, and the moment
/// one of these crates joins the strict job it becomes the same defect that
/// F27 describes.
///
/// This list may SHRINK freely. It may not grow: a file not on it and not
/// environment-gated fails the sweep, which is what keeps the debt bounded
/// while it is paid down.
const UNROUTED_DEBT: &[&str] = &[
    "lunaris-retrieve/tests/hybrid_filter_common/mod.rs",
    "lunaris-retrieve/tests/navigate_filter_moon.rs",
    "lunaris-retrieve/tests/tree_recall.rs",
    "lunaris/tests/chaos_helios_sigkill.rs",
    "lunaris/tests/coding_session_memory_smoke.rs",
    "lunaris/tests/consolidator_scope_isolation.rs",
    "lunaris/tests/phase_14_1_reflect_invalidate.rs",
];

/// Files whose whole job is to name the spelling in order to forbid it.
const GUARDS: &[&str] = &[
    "lunaris-storage-moon/tests/no_silent_moon_skip.rs",
    "lunaris-conformance/tests/no_silent_skip.rs",
    "lunaris-test-harness/tests/no_silent_skip_workspace.rs",
];

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("crates/ is the parent").to_path_buf()
}

/// Source with comment lines removed. Prose describing a skip must never
/// satisfy a check for the presence of one.
fn code_only(body: &str) -> String {
    body.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("/*") && !t.starts_with('*')
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Does this source contain a print that announces a skip?
///
/// Scans each print-macro INVOCATION — from `println!` to its matching close
/// paren, respecting string literals — rather than lines or `;`-delimited
/// spans. Both simpler versions were written first and both had the very bug
/// this function exists to catch:
///
/// * per-LINE missed `run_storage_moon.rs` and `run_protocol_lunaris_server.rs`,
///   which spread one `eprintln!(` and its string across two lines, so the
///   check saw a print with no SKIP and a SKIP with no print;
/// * stopping at the first `;` missed `keyword_bm25.rs`, whose message is
///   literally `"MOON_URL unset; SKIP"` — the delimiter was INSIDE the string.
///
/// Three narrow keys, three blind spots, all of them the F27 shape. Matching
/// the actual token structure is the only version that does not need a fourth
/// correction.
///
/// Scoping to the invocation also keeps `assert!` messages out:
/// `multishard_failfast.rs` says "must be SKIPPED entirely" in one, and an
/// assertion is a FAILURE, not a skip.
fn announces_a_skip(body: &str) -> bool {
    let code = code_only(body);
    let bytes = code.as_bytes();
    let mut i = 0;
    while let Some(rel) = code[i..].find("println!") {
        let macro_start = i + rel;
        let mut j = macro_start;
        // Advance to the opening delimiter.
        while j < bytes.len() && !matches!(bytes[j], b'(' | b'[' | b'{') {
            j += 1;
        }
        let span_start = j;
        let mut depth = 0usize;
        let mut in_str = false;
        let mut escaped = false;
        while j < bytes.len() {
            let c = bytes[j];
            if in_str {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            j += 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            j += 1;
        }
        if code[span_start..j.min(code.len())].contains("SKIP") {
            return true;
        }
        i = macro_start + "println!".len();
    }
    false
}

fn rust_files_under_tests() -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(crates_dir()).expect("read crates/") {
        let p = entry.expect("dir entry").path();
        let tests = p.join("tests");
        if tests.is_dir() {
            stack.push(tests);
        }
    }
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read a tests dir") {
            let p = entry.expect("dir entry").path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                let rel = p
                    .strip_prefix(crates_dir())
                    .expect("under crates/")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, p));
            }
        }
    }
    out
}

fn files_that_announce_a_skip() -> BTreeSet<String> {
    rust_files_under_tests()
        .into_iter()
        .filter(|(_, p)| announces_a_skip(&std::fs::read_to_string(p).unwrap_or_default()))
        .map(|(rel, _)| rel)
        .collect()
}

fn listed() -> BTreeSet<String> {
    ENVIRONMENT_GATED
        .iter()
        .map(|(f, _)| (*f).to_string())
        .chain(UNROUTED_DEBT.iter().map(|f| (*f).to_string()))
        .chain(GUARDS.iter().map(|f| (*f).to_string()))
        .collect()
}

/// The sweep. A new file that decides to skip on its own fails here.
#[test]
fn no_test_file_prints_a_skip_it_decided_alone() {
    let offenders: Vec<String> =
        files_that_announce_a_skip().difference(&listed()).cloned().collect();
    assert!(
        offenders.is_empty(),
        "these files print their own SKIP instead of routing it through a helper that can \
         REFUSE to skip: {offenders:?}\n\n\
         Under LUNARIS_CONFORMANCE_STRICT=1 — which .github/workflows/integration.yml sets at \
         job level — a skip means the fixture broke, and printing one reports success for a \
         suite that tested nothing. Route it through \
         `lunaris_test_harness::strict_skip::note_unavailable` (or a crate-local wrapper \
         over it, as lunaris-storage-moon/tests/common/mod.rs is), or add the file to \
         ENVIRONMENT_GATED with \
         the gate it honours if the missing thing genuinely is not supposed to exist in CI."
    );
}

/// A list naming a file that no longer announces a skip is one guard short:
/// the entry stops meaning anything, and the next file to take that path
/// inherits an exemption nobody granted it.
#[test]
fn every_listed_file_still_announces_a_skip() {
    let actual = files_that_announce_a_skip();
    let stale: Vec<String> = listed()
        .into_iter()
        .filter(|f| !actual.contains(f) && !GUARDS.contains(&f.as_str()))
        .collect();
    assert!(
        stale.is_empty(),
        "these files are listed as skipping but no longer print a SKIP: {stale:?}. If the skip \
         was routed or deleted, remove the entry in the same commit — an exemption outliving \
         the thing it exempted is how a list silently widens."
    );
}

/// The debt list is a ratchet: it may shrink, never grow.
#[test]
fn the_unrouted_debt_has_not_grown() {
    const CEILING: usize = 7;
    assert!(
        UNROUTED_DEBT.len() <= CEILING,
        "UNROUTED_DEBT grew to {} (ceiling {CEILING}). Route the new skip instead of listing \
         it — the list exists to bound debt that predates F27, not to absorb more.",
        UNROUTED_DEBT.len()
    );
}

/// The guards this file backstops must still exist. If one is deleted, this
/// sweep silently becomes the only coverage and its exemptions become the
/// whole policy.
#[test]
fn the_crate_local_guards_still_exist() {
    for g in GUARDS {
        if g.starts_with("lunaris-test-harness/") {
            continue;
        }
        assert!(
            crates_dir().join(g).exists(),
            "{g} is named as a crate-local skip guard but does not exist. This file backstops \
             those guards; it does not replace them."
        );
    }
}

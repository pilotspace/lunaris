//! Every reader of `LUNARIS_CONFORMANCE_STRICT` must agree on what it means.
//!
//! The variable is deliberately ONE switch: `integration.yml` sets it at job
//! level so a suite cannot report success without reaching Moon, and
//! `lunaris-storage-moon`'s helper documents why there is no second name —
//! "an operator could turn half the job strict".
//!
//! A second NAME was never the only way to get there. Four files read this
//! variable independently, and they did not agree on its VALUE:
//! `lunaris/tests/moon_parity.rs` accepted `Ok("1") | Ok("true")`, while the
//! harness, `lunaris-conformance/src/skip.rs` and
//! `lunaris-storage-moon/tests/common/mod.rs` accepted only `"1"` —
//! `lunaris-conformance` with an explicit test saying `=true` must NOT enable
//! it. So `LUNARIS_CONFORMANCE_STRICT=true` turned exactly one suite strict
//! and left three permissive: half the job strict, reached through the parse
//! instead of the name.
//!
//! This guard pins both halves: how many places read it, and that each one
//! compares against exactly `"1"`.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

const VAR: &str = "LUNARIS_CONFORMANCE_STRICT";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate>")
        .to_path_buf()
}

/// Source lines that actually READ the variable from the environment.
///
/// Comments and assertion messages mention it constantly — the sweep must see
/// the `env::var` call, not the name. Keyed on the read, not the word.
fn read_sites() -> Vec<(String, usize, String)> {
    let root = workspace_root().join("crates");
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                if !p.ends_with("target") {
                    stack.push(p);
                }
            } else if p.extension().is_some_and(|x| x == "rs") {
                let Ok(body) = std::fs::read_to_string(&p) else { continue };
                for (n, line) in body.lines().enumerate() {
                    let t = line.trim();
                    if t.starts_with("//") {
                        continue;
                    }
                    if line.contains("env::var") && line.contains(VAR) {
                        let rel = p
                            .strip_prefix(workspace_root())
                            .unwrap_or(&p)
                            .to_string_lossy()
                            .into_owned();
                        // The comparison is not always on the read line. The
                        // harness's own test binds the value first and checks
                        // it on the next line; a single-line window reported
                        // that as a reader with no contract.
                        let lines: Vec<&str> = body.lines().collect();
                        let window = lines[n..(n + 3).min(lines.len())].join(" ");
                        out.push((rel, n + 1, format!("{t} {window}")));
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// Files allowed to read the variable directly. Everything else must route
/// through `lunaris_test_harness::strict_skip::strict()`.
///
/// `lunaris-conformance` keeps its own read because it is a `src/` module in a
/// crate the harness depends on — routing it through the harness would invert
/// the dependency. Its contract is checked below like every other.
const ALLOWED: &[&str] =
    &["crates/lunaris-test-harness/src/strict_skip.rs", "crates/lunaris-conformance/src/skip.rs"];

#[test]
fn only_the_two_owners_read_the_flag_directly() {
    let rogue: Vec<_> =
        read_sites().into_iter().filter(|(f, _, _)| !ALLOWED.contains(&f.as_str())).collect();
    assert!(
        rogue.is_empty(),
        "these files read {VAR} directly instead of calling \
         `lunaris_test_harness::strict_skip::strict()`:\n{}\n\nA private copy is how the \
         readers drifted apart in the first place — one accepted `true`, three did not, so \
         `{VAR}=true` turned a quarter of the job strict. Route through the helper, or add \
         the file to ALLOWED with the dependency reason that forces it.",
        rogue.iter().map(|(f, n, l)| format!("  {f}:{n}\n    {l}")).collect::<Vec<_>>().join("\n")
    );
}

/// Every string literal this read compares the variable against.
///
/// Keyed on the SET of accepted values, not on whether `"1"` is among them.
/// `matches!(.., Ok("1") | Ok("true"))` contains `Ok("1")` — a check for its
/// presence passes the exact divergence this test exists to catch, which is
/// how the first version of this file went green over `moon_parity.rs`.
fn accepted_values(window: &str) -> Vec<String> {
    let mut out = Vec::new();
    for cap in window.split("Ok(\"").skip(1).chain(window.split("Some(\"").skip(1)) {
        if let Some(end) = cap.find('"') {
            out.push(cap[..end].to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn every_reader_treats_exactly_one_as_the_only_true_value() {
    let loose: Vec<_> = read_sites()
        .into_iter()
        .filter(|(_, _, window)| accepted_values(window) != vec!["1".to_string()])
        .map(|(f, n, w)| (f, n, format!("accepts {:?}", accepted_values(&w))))
        .collect();
    assert!(
        loose.is_empty(),
        "these reads of {VAR} do not compare against exactly `Ok(\"1\")`:\n{}\n\n\
         `integration.yml` sets the value `\"1\"`, and \
         `lunaris-conformance/src/skip.rs` documents that `=true` must NOT enable strict \
         mode. A reader that accepts more values makes the switch mean different things \
         in different suites.",
        loose.iter().map(|(f, n, l)| format!("  {f}:{n}\n    {l}")).collect::<Vec<_>>().join("\n")
    );
}

/// Vacuity floor. Both tests above pass on an empty scan — no read sites means
/// no rogue sites and no loose comparisons. Pins that the scan finds the
/// owners it is built around.
#[test]
fn the_scan_finds_the_reads_it_is_meant_to_check() {
    let sites = read_sites();
    assert!(
        sites.len() >= 2,
        "expected at least the two ALLOWED owners to read {VAR}; the scan found \
         {}: {sites:?}. If it found none, both tests above are asserting nothing.",
        sites.len()
    );
    for owner in ALLOWED {
        assert!(
            sites.iter().any(|(f, _, _)| f == owner),
            "{owner} is listed as an owner of the {VAR} read but the scan did not find a \
             read there; got {sites:?}"
        );
    }
}

//! A test that spawns a Lunaris binary must close the contextd route.
//!
//! ## The bug this exists to prevent
//!
//! Every surface (`lunaris` CLI, `lunaris-hook`, `lunaris-mcp`) resolves its
//! store **socket-first**: if a contextd is reachable it serves the call from
//! ITS store, and the `LUNARIS_STORE_URL` / `LUNARIS_MCP_STORAGE` the test set
//! is never consulted. Setting a store URL therefore looks like isolation and
//! is not — the daemon's socket outranks it.
//!
//! Measured 2026-09-01 on a developer machine running contextd against the live
//! personal store: `record_decision_smoke` and `record_edit_smoke` had written
//! **265** and **251** keys into that store under `test-record-decision` /
//! `test-record-edit`. A production census of curated memories found exactly
//! those rows and nothing else — the suite had manufactured the only evidence
//! that anything was ever curated. `repair_vectors_end_to_end` leaked in the
//! read direction: its preview printed `(via contextd)` and `scanned=0`,
//! having walked the developer's store instead of the Moon it had just seeded.
//!
//! The failure is silent by construction. A leaking test still passes — these
//! two were caught only because the live store had wedged on `diskfull` and
//! rejected their writes.
//!
//! ## What is asserted
//!
//! Keyed on the *decision* ("this test drives a binary at a store it chose"),
//! not on one spelling of it: any test source that spawns one of the three
//! socket-first binaries AND names a store must also close the socket, by any
//! of the three sanctioned means.

use std::path::{Path, PathBuf};

/// The binaries whose store resolution is socket-first.
const SOCKET_FIRST: [&str; 3] =
    ["CARGO_BIN_EXE_lunaris\"", "CARGO_BIN_EXE_lunaris-hook", "CARGO_BIN_EXE_lunaris-mcp"];

/// Naming a store is what makes an unclosed socket a *lie* rather than a
/// default: the test has stated where its data belongs.
const NAMES_A_STORE: [&str; 3] = ["LUNARIS_STORE_URL", "LUNARIS_MCP_STORAGE", "LUNARIS_HOOK_STORE"];

/// Three sanctioned ways to close the route. `DISABLE_CONTEXTD` is the MCP
/// server's own flag; pinning the socket at an impossible path works for every
/// binary; a tempdir `HOME` works only when the env var is also cleared, since
/// it outranks `HOME` — so that spelling must show both.
fn closes_the_socket(src: &str) -> bool {
    src.contains("LUNARIS_MCP_DISABLE_CONTEXTD")
        || src.contains("LUNARIS_CONTEXTD_SOCKET")
        || (src.contains(".env(\"HOME\"")
            && src.contains("env_remove(\"LUNARIS_CONTEXTD_SOCKET\")"))
}

fn tests_dirs() -> Vec<PathBuf> {
    let ws = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("crates/").to_path_buf();
    ["lunaris-mcp", "lunaris-cli", "lunaris-hook"]
        .iter()
        .map(|c| ws.join(c).join("tests"))
        .filter(|p| p.is_dir())
        .collect()
}

#[test]
fn every_binary_spawning_test_closes_the_contextd_route() {
    let dirs = tests_dirs();
    assert_eq!(dirs.len(), 3, "all three crates must be scanned; got {dirs:?}");

    let mut scanned = 0_usize;
    let mut offenders = Vec::new();

    for dir in dirs {
        for entry in std::fs::read_dir(&dir).expect("read tests dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // This file names every symbol it guards; scanning itself would
            // make the ratchet its own counter-example.
            if path.file_name().and_then(|n| n.to_str()) == Some("contextd_isolation_ratchet.rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("read test source");
            let spawns = SOCKET_FIRST.iter().any(|b| src.contains(b));
            let names_store = NAMES_A_STORE.iter().any(|v| src.contains(v));
            if !(spawns && names_store) {
                continue;
            }
            scanned += 1;
            if !closes_the_socket(&src) {
                offenders.push(path.display().to_string());
            }
        }
    }

    // Anti-vacuity: if the scan matches nothing, the guard is green for the
    // wrong reason. The population was 9 files when this was written.
    assert!(
        scanned >= 8,
        "the ratchet matched only {scanned} test files — the spawn or store \
         markers have drifted and this guard is no longer looking at anything"
    );

    assert!(
        offenders.is_empty(),
        "these tests drive a Lunaris binary at a store they named, but leave the \
         contextd socket open — on any machine running contextd they will be served \
         by the developer's live store, silently, and still pass:\n  {}",
        offenders.join("\n  ")
    );
}

//! Skip discipline: in CI, a skip is a failure.
//!
//! Every live-backend runner in this crate short-circuits to `Ok(())` when its
//! backend is unreachable, so a developer without a Moon gets a green suite
//! instead of a wall of connection errors. That is the right default locally
//! and exactly the wrong one in CI, where `integration.yml` guarantees the
//! preconditions: there, a skip means the job stopped testing and still
//! reported success.
//!
//! This is not hypothetical for this crate. The `/v1/forget` P0 — 200 OK and
//! nothing deleted, for every real tenant — is guarded by exactly one thing:
//! `protocol::forget::two_step_hard_delete`, which runs only inside
//! `run_protocol_lunaris_server.rs`. That runner has THREE paths that print a
//! SKIP and return `Ok(())`. Any one of them firing in CI leaves the P0
//! completely unguarded behind a green check.
//!
//! So `LUNARIS_CONFORMANCE_STRICT=1` turns each skip into a failure, CI sets
//! it, and the two tests below hold both halves: the runners consult it, and
//! the workflow actually sets it. Neither half is worth anything alone — a
//! strict mode nothing enables is inert, and an env var no runner reads is
//! decoration.

use std::path::{Path, PathBuf};

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Every file that owns a skip decision — the point where a runner reports
/// success without having tested.
///
/// A roster rather than a pattern scan, deliberately. The obvious guard is
/// "no file may contain a bare `return Ok(())` skip", and it is worthless:
/// once the skips are routed that shape is gone, so the scan matches nothing
/// and stays green forever no matter what regresses. The roster asserts the
/// positive invariant instead — these specific files route — and cannot go
/// vacuous, because a missing entry fails on the spot.
const ROSTER: &[&str] = &[
    "tests/run_protocol_lunaris_server.rs",
    "tests/run_storage_moon.rs",
    "tests/crash_recovery.rs",
    "src/bindings/mod.rs",
];

/// The module every skip decision must route through, in either spelling:
/// `skip::skip_or_fail` for an `anyhow` runner, `skip::strict` for a proptest
/// body that owes a `TestCaseResult`.
const ROUTED: &str = "skip::";

/// `skip.rs` owns the only SKIP message in the crate. Any other file emitting
/// one is announcing a skip it decided on its own.
const UNROUTED_SPELLING: &str = "eprintln!(\"SKIP";

fn code_only(body: &str) -> String {
    body.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with('*')
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collects EVERY offender rather than failing on the first. A guard that
/// stops at the first entry passes a mutation of the second one by accident.
#[test]
fn every_known_skip_decision_routes_through_strict_mode() {
    let root = crate_root();
    let mut missing = Vec::new();
    let mut offenders = Vec::new();

    for rel in ROSTER {
        let path = root.join(rel);
        if !path.exists() {
            missing.push(*rel);
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("read source");
        if !code_only(&body).contains(ROUTED) {
            offenders.push(*rel);
        }
    }

    assert!(
        missing.is_empty(),
        "these rostered runners no longer exist: {missing:?}. If one was deleted or \
         renamed, update ROSTER deliberately — silently dropping an entry is how this \
         guard would stop covering the thing it was written for."
    );
    assert!(
        offenders.is_empty(),
        "these runners can report a green result without consulting strict mode: \
         {offenders:?}. In CI every precondition is satisfied by construction, so a \
         skip there means the job silently stopped testing. Route the decision through \
         `lunaris_conformance::skip::skip_or_fail` (or `skip::strict()` inside a \
         proptest body)."
    );
}

/// Catches a NEW unrouted skip, which the roster by definition cannot: a file
/// nobody has added to it yet.
#[test]
fn no_runner_announces_a_skip_of_its_own() {
    let root = crate_root();
    let mut offenders = Vec::new();

    for dir in ["tests", "src", "src/bindings", "src/protocol"] {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            // `skip.rs` emits the one legitimate SKIP message; this file names
            // the spelling in order to forbid it.
            if name == "skip.rs" || name == "no_silent_skip.rs" {
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("read source");
            if code_only(&body).contains(UNROUTED_SPELLING) {
                offenders.push(format!("{dir}/{name}"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these files announce a skip without routing it through \
         `lunaris_conformance::skip`: {offenders:?}. A skip printed directly is a skip \
         that stays green in CI, where every precondition is guaranteed and a skip \
         therefore means the suite stopped testing."
    );
}

/// The other half: strict mode that CI never turns on protects nothing.
#[test]
fn the_integration_workflow_enables_strict_mode() {
    let workflow = crate_root()
        .join("../../.github/workflows/integration.yml")
        .canonicalize()
        .expect("integration.yml must exist — it is the only job with a live Moon");
    let body = std::fs::read_to_string(workflow).expect("read integration.yml");

    assert!(
        body.contains("LUNARIS_CONFORMANCE_STRICT: \"1\""),
        "integration.yml no longer sets LUNARIS_CONFORMANCE_STRICT=\"1\". That job is \
         the one place every conformance precondition is guaranteed, so it is the one \
         place a skip is unambiguously a defect. Without it the runners fall back to \
         skipping quietly and the suite can report success having asserted nothing. \
         The value must be exactly \"1\" — `skip::strict()` accepts nothing else."
    );
}

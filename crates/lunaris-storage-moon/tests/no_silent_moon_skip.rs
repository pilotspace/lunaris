//! Guard: this crate's live-Moon skip decision lives in exactly one place,
//! and that place refuses to skip when the environment promised a Moon.
//!
//! Two halves, because a structural check alone would not be enough. The
//! roster below proves no file re-grows its own private skip; the behavioural
//! tests prove the shared helper actually panics — a scanner that only reads
//! source cannot tell a working strict mode from a decorative one.

mod common;

/// Every file that owns a live-Moon skip decision.
///
/// A roster, not a pattern sweep. The obvious guard — "no file contains a
/// bare `eprintln!(\"MOON_URL not reachable\")`" — goes VACUOUS the moment the
/// fix lands: after routing, the spelling appears nowhere, so the guard
/// matches nothing and stays green forever, including on the day someone adds
/// a fifteenth file with a fresh copy of the old helper. Naming the files
/// means deleting one is a deliberate edit here, and adding one that skips
/// without routing is caught by the sweep below.
/// Four of these eighteen were NOT found by grepping for `connect_or_skip`:
/// `keyword_bm25.rs` and `scope_isolation.rs` decide inline in a `match`
/// arm, `list_scopes.rs` calls its helper `maybe_connect`, and
/// `multishard_live.rs` skips on a raw redis handshake. The sweep below found
/// them on this guard's first RED — which is the argument for having both
/// halves rather than a roster alone. `list_scopes.rs` and
/// `scope_isolation.rs` matter most: they hold the six
/// `#[ignore = "requires live Moon"]` tests that integration.yml deliberately
/// un-ignores with `--include-ignored`, so CI went out of its way to run
/// exactly the tests that would have skipped silently.
///
/// Six more joined on 2026-08-23, and how they were missed is the point. They
/// reach Moon through `EphemeralMoon::spawn()` rather than a `connect_or_skip`
/// helper, and announce it as `no ephemeral Moon (..); SKIP` — so the sweep
/// below, which then required the literal phrase "not reachable", matched none
/// of them. A guard keyed on one family's wording is blind to the next family,
/// and this one was blind to `valid_time_half_open.rs` and
/// `zero_vector_not_indexed.rs`: the F21 and F22 guards, both of them running
/// in the strict integration job, both of them green if Moon never came up.
const ROSTER: &[&str] = &[
    "a_hybrid_filter_trust.rs",
    "a_maintenance_compact.rs",
    "a_quant_ef_guardrails.rs",
    "b_graph_hotpath.rs",
    "concurrent_txn_isolation.rs",
    "cypher_ingest_hazards.rs",
    "dim_configurable.rs",
    "graph_anchor_constrains.rs",
    "graph_decay_recency.rs",
    "hotkeys_live.rs",
    "keyword_bm25.rs",
    "knn_prefilter_is_never_silently_dropped.rs",
    "list_scopes.rs",
    "mq_backlog_delivery.rs",
    "mq_stranded_recovery.rs",
    "mq_typed_client.rs",
    "multishard_live.rs",
    "navigate_ab_bench.rs",
    "navigate_recall.rs",
    "quantization_recall.rs",
    "scope_isolation.rs",
    "valid_time_half_open.rs",
    "vector_filter_moon.rs",
    "zero_vector_not_indexed.rs",
];

fn tests_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// Code only — a comment may quote the old spelling freely.
///
/// The conformance crate's sibling guard passed on its first RED because a
/// workflow COMMENT satisfied the check it was making. Prose describing an
/// absence must never satisfy a presence check.
fn code_of(path: &std::path::Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("/*") && !t.starts_with('*')
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A code line that PRINTS a skip. The decision's observable form.
fn announces_a_skip(line: &str) -> bool {
    (line.contains("eprintln!") || line.contains("println!")) && line.contains("SKIP")
}

#[test]
fn every_live_moon_skip_routes_through_the_strict_helper() {
    let dir = tests_dir();
    let mut unrouted = Vec::new();
    for name in ROSTER {
        let p = dir.join(name);
        assert!(
            p.exists(),
            "{name} is on the live-Moon skip roster but no longer exists. If it \
             was deleted on purpose, remove it from ROSTER in the same commit — \
             a roster that names a missing file is one silent-skip guard short."
        );
        let code = code_of(&p);
        if code.contains("SKIP") && !code.contains("common::note_moon_unreachable") {
            unrouted.push(*name);
        }
    }
    assert!(
        unrouted.is_empty(),
        "these files decide to skip a live-Moon test without routing through \
         common::note_moon_unreachable: {unrouted:?}. Under \
         LUNARIS_CONFORMANCE_STRICT=1 they report success for a suite that \
         never reached Moon — which is exactly what integration.yml's own \
         comment forbids."
    );
}

/// A fifteenth file that grows its own private skip must be caught too — the
/// roster above only polices files it already knows about.
#[test]
fn no_test_file_outside_the_roster_invents_its_own_skip() {
    let dir = tests_dir();
    let mut rogue = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("read tests/") {
        let p = entry.expect("dir entry").path();
        if p.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        // This guard's own source names the helper and the spelling.
        if name == "no_silent_moon_skip.rs" || ROSTER.contains(&name.as_str()) {
            continue;
        }
        let code = code_of(&p);
        // Keyed on the skip DECISION, not on one phrasing of it. The first
        // version of this sweep required the literal "not reachable"
        // alongside "SKIP", which matched the `connect_or_skip` family and
        // nothing else — so the six files that reach Moon through
        // `EphemeralMoon::spawn()` and print `no ephemeral Moon (..); SKIP`
        // sailed past a guard written to catch exactly them. Two of the six
        // were the F21 and F22 guards: the strict integration job was running
        // them, and would have reported success on a Moon that never came up.
        // The key is a PRINT that announces a skip — `eprintln!(".. SKIP")` —
        // which is the observable form of the decision in all six, whatever
        // words surround it. Bare `SKIP` was tried first and over-matched:
        // `multishard_failfast.rs` carries "must be SKIPPED entirely" inside
        // an `assert!` message, and an assertion is a FAILURE, not a skip.
        // `code_of` has already dropped comment lines, so prose describing a
        // skip cannot trip it either.
        if code.lines().any(announces_a_skip) {
            rogue.push(name);
        }
    }
    assert!(
        rogue.is_empty(),
        "{rogue:?} skip on an unreachable Moon without routing through \
         common::note_moon_unreachable, and are not on ROSTER. Add the call \
         and the file to ROSTER, or make the test fail loudly like \
         episode_roundtrip and moon_client_smoke already do."
    );
}

/// The structural half above cannot tell a working strict mode from a
/// decorative one. This is the half that can.
///
/// Note what is NOT here: any mutation of the process environment. The flag
/// is a parameter precisely so this test cannot race a sibling in the same
/// binary — see the note on `note_moon_unreachable_with`.
#[test]
fn strict_mode_refuses_to_skip() {
    let panicked = std::panic::catch_unwind(|| {
        common::note_moon_unreachable_with("connection refused", true);
    })
    .is_err();
    assert!(
        panicked,
        "note_moon_unreachable_with(.., strict = true) returned instead of \
         panicking. Every routed skip site in this crate then reports success \
         with an unreachable Moon, in the one job that guarantees a reachable one."
    );
}

#[test]
fn a_dev_box_still_skips() {
    common::note_moon_unreachable_with("connection refused", false);
    // Reaching here IS the assertion: without a Moon on a developer's laptop
    // the suite must stay skippable, or nobody can run any other test in the
    // crate. A strict mode that cannot be turned off gets turned off wholesale.
}

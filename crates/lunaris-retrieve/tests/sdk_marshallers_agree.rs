//! F14 — the two SDK marshallers and the Rust parser must name the same ops.
//!
//! `lunaris_retrieve::plan::retriever_from_json` is the single parser, but each
//! SDK has its own marshaller that writes the JSON it consumes:
//! `crates/lunaris-py/python/lunaris/dsl.py::_collapse_plan` and
//! `crates/lunaris-ts/lunaris.cjs::_collapsePlan`. The code comments in both
//! say "keep the two in step; a divergence here is an SDK parity bug" — this
//! test is what makes that sentence load-bearing.
//!
//! Every drift mode here is loud rather than silent (an op only Python emits
//! hits the parser's `UnknownOp`; an op only the parser knows is unreachable
//! from that SDK), so nothing here can return wrong ANSWERS. What it catches is
//! a capability that silently exists in one SDK and not the other — which is
//! exactly what F14 was filed for.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// The ops the Rust parser has a `match` arm for.
///
/// Keyed on the arm's literal pattern rather than on any list a human keeps
/// in sync: a list would drift from the `match` without either one being
/// wrong on its own, which is the failure this whole test exists to catch.
fn parser_ops() -> BTreeSet<String> {
    let src = read("crates/lunaris-retrieve/src/plan.rs");
    let body = src.split_once("match op {").expect("plan.rs still dispatches on `match op {`").1;
    let body = body.split_once("\n    }").expect("the match block ends").0;
    body.lines()
        .map(str::trim)
        .filter_map(|l| l.strip_prefix('"'))
        .filter_map(|l| l.split_once('"'))
        .map(|(op, _)| op.to_string())
        .collect()
}

/// The ops an SDK marshaller emits, read off the `"op": "<name>"` /
/// `op: "<name>"` literals in the object it builds.
fn emitted_ops(src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for pat in ["\"op\": \"", "op: \""] {
        for tail in src.split(pat).skip(1) {
            if let Some((op, _)) = tail.split_once('"') {
                out.insert(op.to_string());
            }
        }
    }
    // Both marshallers write `{"op": n.op, ...}` for the vector/keyword pair
    // rather than repeating the literal, so those two never appear as string
    // literals. Recover them from the branch that does it.
    for op in ["vector", "keyword"] {
        if src.contains(&format!("\"{op}\"")) {
            out.insert(op.to_string());
        }
    }
    out
}

#[test]
fn both_sdk_marshallers_emit_exactly_the_ops_the_parser_builds() {
    let parser = parser_ops();
    let py = emitted_ops(&read("crates/lunaris-py/python/lunaris/dsl.py"));
    let ts = emitted_ops(&read("crates/lunaris-ts/lunaris.cjs"));

    assert_eq!(
        py, parser,
        "the Python marshaller and the Rust plan parser disagree on the operator \
         set. Python emits {py:?}; the parser builds {parser:?}. An op only \
         Python emits is a hard error at recall; an op only the parser knows is \
         a capability Python users cannot reach."
    );
    assert_eq!(
        ts, parser,
        "the TypeScript marshaller and the Rust plan parser disagree on the \
         operator set. TypeScript emits {ts:?}; the parser builds {parser:?}."
    );
}

/// Vacuity floor. Both assertions above compare sets scraped out of source, so
/// they are only meaningful if the scrapes actually find something. A regex
/// that silently matched nothing would make two empty sets compare equal and
/// the test would pass while checking nothing.
#[test]
fn the_scrapes_find_the_operators_they_are_meant_to_check() {
    let parser = parser_ops();
    assert!(
        parser.len() >= 6,
        "the parser scrape found only {parser:?} — plan.rs's `match op` shape changed"
    );
    for expected in ["and", "fuse_rrf", "graph", "keyword", "top", "vector"] {
        assert!(parser.contains(expected), "parser scrape missed `{expected}`: {parser:?}");
    }
    assert!(
        emitted_ops(&read("crates/lunaris-py/python/lunaris/dsl.py")).contains("fuse_rrf"),
        "the Python scrape found no `fuse_rrf` — _collapse_plan's emit shape changed"
    );
    assert!(
        emitted_ops(&read("crates/lunaris-ts/lunaris.cjs")).contains("fuse_rrf"),
        "the TypeScript scrape found no `fuse_rrf` — _collapsePlan's emit shape changed"
    );
}

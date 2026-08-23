//! F23 — the files the SDKs COMPILE must match the codegen snapshot.
//!
//! ## The gap
//!
//! `lunaris-codegen --emit` writes only to `crates/lunaris-codegen/snapshots/`,
//! and `--check` diffs only those same three paths ([`codegen_managed_paths`]).
//! The files the SDKs actually build against —
//! `crates/lunaris-ts/src/generated.rs` and `crates/lunaris-py/src/generated.rs`
//! — are copied across BY HAND, and until this file nothing compared them.
//!
//! So a contributor who edits the emitter, regenerates, and forgets the copy
//! gets a green `parity-check` over an SDK still containing the old code. The
//! job's own name says the opposite of what it verifies.
//!
//! ## Why this is not a byte comparison
//!
//! It cannot be. On `main` today `lunaris-ts/src/generated.rs` is 795 lines and
//! its snapshot is 695 — 226 differing lines — because the shipped copy has
//! been through `rustfmt` and the snapshot has not. Normalise both and the
//! difference is **zero**: the divergence is entirely whitespace.
//!
//! A byte-compare guard would therefore be RED on arrival for a benign reason,
//! and a guard that is red for a benign reason gets re-copied around or
//! switched off. So the Rust pair are compared as PARSED SYNTAX TREES, re-
//! printed through `prettyplease` — two files agree when they mean the same
//! thing, whatever their formatting.
//!
//! The `.d.ts` has no Rust parser available and is byte-identical to its
//! snapshot today, so it is compared after normalising line endings and
//! trailing whitespace only. If that ever starts producing benign diffs, it
//! needs a real TypeScript-aware comparison rather than a loosened assertion.

use std::path::{Path, PathBuf};

/// (shipped, snapshot). The shipped path is the one a `cargo build` of the SDK
/// actually reads; the snapshot is what `--emit` writes.
const RUST_PAIRS: &[(&str, &str)] = &[
    ("crates/lunaris-ts/src/generated.rs", "crates/lunaris-codegen/snapshots/generated_ts.rs"),
    ("crates/lunaris-py/src/generated.rs", "crates/lunaris-codegen/snapshots/generated_py.rs"),
];

// There is no TEXT_PAIRS list. `crates/lunaris-ts/generated.d.ts` used to be a
// hand-copy of the `.d.ts` snapshot and was the third pair here; it was deleted
// with F17 because nothing consumed it — absent from `tsconfig.json`'s
// `include` and from `package.json`'s `files`, so neither type-checked nor
// published, while declaring a snake_case surface the runtime does not expose.
// The `.d.ts` snapshot is now pinned against napi's own `index.d.ts` by
// `dts_matches_napi_naming.rs`, which checks something a copy-compare cannot:
// that the declared names EXIST.

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate>")
        .to_path_buf()
}

fn read(root: &Path, rel: &str) -> String {
    let p = root.join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Re-print Rust source from its parsed AST, so formatting cannot make two
/// equivalent files look different.
fn normalize_rust(src: &str, what: &str) -> String {
    let file = syn::parse_file(src)
        .unwrap_or_else(|e| panic!("{what} is not parseable Rust: {e}. A generated file that does not parse is a bug in the emitter, not in this test."));
    prettyplease::unparse(&file)
}

#[test]
fn every_shipped_rust_file_means_the_same_as_its_snapshot() {
    let root = workspace_root();
    for (shipped, snapshot) in RUST_PAIRS {
        let a = normalize_rust(&read(&root, shipped), shipped);
        let b = normalize_rust(&read(&root, snapshot), snapshot);
        assert_eq!(
            a, b,
            "{shipped} and {snapshot} do not agree once formatting is normalised.\n\n\
             The snapshot is what `lunaris-codegen --emit` produces; the shipped file is what \
             the SDK compiles. `--check` only ever looked at the snapshot, so `parity-check` \
             goes green while the SDK carries different code. Re-copy the snapshot over the \
             shipped path (and rustfmt it) in the same commit that regenerates it.\n\n\
             This is NOT a whitespace complaint — both sides were re-printed through \
             prettyplease before comparing, so a difference here is a difference in meaning."
        );
    }
}

/// Anti-noise floor, and the reason this guard is AST-based.
///
/// The shipped TS glue is rustfmt'd and its snapshot is not, so the two differ
/// by ~226 raw lines while meaning the same thing. If a future change made the
/// two byte-identical, that is fine — but if this test ever fails it means the
/// pair are byte-equal AND the guard above has stopped being able to tell the
/// difference between formatting and meaning, which is worth knowing.
#[test]
fn the_guard_is_doing_more_than_a_byte_compare() {
    let root = workspace_root();
    let (shipped, snapshot) = RUST_PAIRS[0];
    let raw_equal = read(&root, shipped) == read(&root, snapshot);
    let normalized_equal = normalize_rust(&read(&root, shipped), shipped)
        == normalize_rust(&read(&root, snapshot), snapshot);
    assert!(
        normalized_equal,
        "the pair must agree after normalisation — that is the invariant above"
    );
    assert!(
        !raw_equal,
        "{shipped} and {snapshot} are now byte-identical. That is not a failure in itself, but \
         this test exists to document WHY the comparison is AST-based: on 2026-08-23 the pair \
         differed by 226 raw lines purely because one had been rustfmt'd. If they are byte-equal \
         now, either the emit pipeline started formatting its output (good — simplify this guard \
         to a byte compare and delete this test) or someone re-copied one over the other without \
         regenerating. Check which before editing."
    );
}

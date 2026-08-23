//! The emitted `.d.ts` must DECLARE the surface napi-rs actually exports (F17).
//!
//! `emit_ts` produces two things from one IR: Rust glue that napi consumes, and
//! a `.d.ts` that describes what napi will hand JavaScript. Those two have
//! different naming rules. The glue keeps snake_case — it is napi's INPUT, and
//! renaming it changes the exported name. The declarations must be camelCase,
//! because that is what napi emits.
//!
//! Until 2026-08-23 the declaration half skipped the rename, so the snapshot
//! declared `with_graph_pipeline`, `ingest_ticket`, `as_of` and five more
//! methods that do not exist at runtime. Nothing caught it: the file is not in
//! `tsconfig.json`'s `include`, not in `package.json`'s `files`, and no spec
//! calls a snake_case method — so it type-checked nothing and shipped nowhere.
//! It was wrong only to a human reading the tree, which is the failure mode
//! that survives longest.
//!
//! The check here is a cross-reference, not a spelling rule: every method name
//! the emitter declares must appear in `crates/lunaris-ts/index.d.ts`, which
//! napi-rs generates from the real bindings. A rule ("camelCase it") can be
//! satisfied by a wrong name that happens to be camel; only the cross-reference
//! catches a name napi does not actually export.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate>")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Method and function names declared in a `.d.ts`, in declaration position:
/// an indented `name(` or `static name(`.
fn declared_names(dts: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in dts.lines() {
        let t = line.trim_start();
        if t.len() == line.len() {
            continue; // not indented — not a member declaration
        }
        let t = t.strip_prefix("static ").unwrap_or(t);
        let Some(paren) = t.find('(') else { continue };
        let name = &t[..paren];
        if !name.is_empty()
            && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            out.push(name.to_string());
        }
    }
    out
}

const EMITTED: &str = "crates/lunaris-codegen/snapshots/generated_ts.d.ts";
const NAPI: &str = "crates/lunaris-ts/index.d.ts";

#[test]
fn every_declared_method_is_one_napi_actually_exports() {
    let emitted = read(EMITTED);
    let napi = read(NAPI);
    let missing: Vec<String> = declared_names(&emitted)
        .into_iter()
        .filter(|n| !napi.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|w| w == n))
        .collect();
    assert!(
        missing.is_empty(),
        "{EMITTED} declares {} method(s) that do not appear in {NAPI}, which napi-rs \
         generates from the real bindings: {missing:?}. A declaration for a method that \
         does not exist is worse than no declaration — it type-checks calls that fail at \
         runtime. If napi's naming changed, regenerate with \
         `cargo run -p lunaris-codegen -- --emit ts`.",
        missing.len()
    );
}

#[test]
fn the_emitted_declarations_are_not_snake_case() {
    let emitted = read(EMITTED);
    let snake: Vec<String> = declared_names(&emitted)
        .into_iter()
        .filter(|n| n.trim_matches('_').contains('_'))
        .collect();
    assert!(
        snake.is_empty(),
        "{EMITTED} declares snake_case member(s): {snake:?}. napi-rs renames every method \
         and parameter to lowerCamelCase before it reaches JavaScript, so these name \
         methods that do not exist (F17)."
    );
}

/// Vacuity floor. Both tests above pass trivially on an empty name list — a
/// scanner that matched nothing would report a clean bill of health for a file
/// full of wrong names. This pins that the scanner sees a realistic surface.
#[test]
fn the_scanner_actually_finds_the_declarations() {
    let names = declared_names(&read(EMITTED));
    assert!(
        names.len() >= 20,
        "expected the emitted .d.ts to declare at least 20 members; the scanner found {} \
         ({names:?}). Either the emitter shrank drastically or `declared_names` stopped \
         matching the file's shape — in which case the other two tests in this file are \
         asserting nothing.",
        names.len()
    );
    assert!(
        names.iter().any(|n| n == "asOf"),
        "expected `asOf` among the declared members; got {names:?}. This is the canonical \
         renamed method — if it is absent the scan is not reading the file it thinks it is."
    );
}

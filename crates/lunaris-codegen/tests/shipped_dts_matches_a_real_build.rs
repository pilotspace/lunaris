//! `crates/lunaris-ts/index.d.ts` is a committed build artifact. It must
//! describe the build we SHIP, and it must describe a build that can exist.
//!
//! Two ways it stops doing that, both observed:
//!
//! 1. **Test-only surface leaks in.** `napi build --features bindings-it` —
//!    which every local test workflow needs — adds the `conformance.rs`
//!    helpers to `index.d.ts`. Whoever runs that and commits the result
//!    publishes test-only API into the npm type definitions. Nothing caught
//!    this; it was noticed once because a diff happened to be reviewed
//!    (F24).
//!
//! 2. **Mutually exclusive `cfg` arms BOTH land.** `#[napi(factory)]` written
//!    ABOVE `#[cfg(feature = "…")]` registers the item with napi's codegen
//!    before cfg-stripping removes it, so a `#[cfg(feature = "llamacpp")]` fn
//!    and its `#[cfg(not(feature = "llamacpp"))]` stub are emitted as two
//!    identical declarations. `tsc` accepts that as an overload pair, so
//!    nothing downstream complains — but the file then describes a build that
//!    is impossible, and the honest signal that a Tier-0 build drops the
//!    factory is gone. Fix is attribute order: `#[cfg]` first.
//!
//! Both are cheap to check statically, which matters because the alternative
//! — rebuilding with the production feature set and diffing — needs a full
//! llama.cpp compile and so cannot sit on the PR path.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate>")
        .to_path_buf()
}

fn dts() -> String {
    let p = repo_root().join("crates/lunaris-ts/index.d.ts");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// JS names every `#[napi]` export in `conformance.rs` presents.
///
/// Derived from the source, not hardcoded: the whole file is
/// `#![cfg(feature = "bindings-it")]`, so anything exported there is test-only
/// by construction, and a helper added later is covered with no edit here.
fn test_only_exports() -> Vec<String> {
    let p = repo_root().join("crates/lunaris-ts/src/conformance.rs");
    let body = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
    let mut out = Vec::new();
    for line in body.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("#[napi(js_name = \"")
            && let Some(end) = rest.find('"')
        {
            out.push(rest[..end].to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

#[test]
fn no_test_only_export_reaches_the_shipped_declarations() {
    let dts = dts();
    let leaked: Vec<String> = test_only_exports().into_iter().filter(|n| dts.contains(n)).collect();
    assert!(
        leaked.is_empty(),
        "these `bindings-it`-only exports appear in the committed \
         crates/lunaris-ts/index.d.ts: {leaked:#?}\n\nThey exist only under a feature production \
         npm packages never enable, so the artifact was built with --features bindings-it and \
         committed. Rebuild with the production feature set (plain `napi build`) before \
         committing index.d.ts / index.js."
    );
}

/// `(class, member)` pairs declared more than once in one class body.
fn duplicate_members(src: &str) -> Vec<(String, String)> {
    let mut current: Option<String> = None;
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut dups = Vec::new();
    for line in src.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("export declare class ") {
            current = Some(rest.split_whitespace().next().unwrap_or(rest).to_string());
            continue;
        }
        if t == "}" {
            current = None;
            continue;
        }
        let Some(class) = current.as_ref() else { continue };
        let decl = t
            .strip_prefix("static ")
            .or_else(|| t.strip_prefix("get "))
            .or_else(|| t.strip_prefix("set "))
            .unwrap_or(t);
        let Some(paren) = decl.find('(') else { continue };
        let name = decl[..paren].trim();
        if name.is_empty() || name == "constructor" || !name.chars().all(|c| c.is_alphanumeric()) {
            continue;
        }
        let key = (class.clone(), name.to_string());
        if seen.contains(&key) {
            dups.push(key);
        } else {
            seen.push(key);
        }
    }
    dups
}

#[test]
fn no_member_is_declared_twice() {
    let dups = duplicate_members(&dts());
    assert!(
        dups.is_empty(),
        "these members are declared twice in crates/lunaris-ts/index.d.ts: {dups:#?}\n\n\
         The usual cause is `#[napi(...)]` written ABOVE `#[cfg(feature = \"…\")]`: napi's \
         codegen registers the item before cfg-stripping, so a feature arm and its \
         `#[cfg(not(...))]` stub BOTH reach the declarations. Put `#[cfg]` first. `tsc` \
         accepts the pair as overloads, so nothing downstream will tell you — but the file \
         then describes a build that cannot exist."
    );
}

/// Vacuity floor. Both tests above pass over an empty scan: no test-only
/// exports found means nothing can leak, and an unparsed `.d.ts` has no
/// duplicate members.
#[test]
fn the_scans_find_what_they_are_meant_to_check() {
    let exports = test_only_exports();
    assert!(
        exports.len() >= 3,
        "expected at least the three known conformance helpers in conformance.rs; \
         found {exports:?}. If the scan finds none, the leak test asserts nothing."
    );
    let dts = dts();
    assert!(
        dts.contains("export declare class EmbedderConfig"),
        "index.d.ts has no EmbedderConfig class; the duplicate-member parse is reading \
         something unexpected and asserts nothing"
    );
    // The duplicate parse must be able to SEE members at all.
    let mut probe =
        String::from("export declare class Probe {\n  static a(): void\n  static a(): void\n}\n");
    probe.push('\n');
    assert_eq!(
        duplicate_members(&probe),
        vec![("Probe".to_string(), "a".to_string())],
        "the duplicate-member parse failed on a hand-built duplicate, so a real one \
         would slip through"
    );
}

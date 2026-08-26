//! `crates/lunaris-ts/index.d.ts` is a committed build artifact, and nothing
//! rebuilds it on the PR path — a full `napi build` needs a llama.cpp compile.
//!
//! Its sibling `dts_matches_napi_naming.rs` cross-references the CODEGEN
//! snapshot against it, which covers every method the emitter produces. It
//! does not cover the handwritten `#[napi]` surfaces —
//! `crates/lunaris-ts/src/{scope,toggles,open_overrides,embedder_config,
//! reranker_config,dsl,lib}.rs` — because those never pass through the
//! emitter at all.
//!
//! So a `#[napi]` method added to `scope.rs` by hand, without re-running
//! `napi build` and committing the result, exists in Rust, is absent from the
//! shipped type definitions, and is invisible to every TypeScript caller —
//! with the whole board green. That is the F3 family exactly: a committed
//! artifact drifting from the source it claims to describe, in the direction
//! nothing was watching. (Found landing the Wave 6 retention methods, where
//! the regeneration was a step someone had to REMEMBER.)
//!
//! This is the source -> dts direction. It is static, so it costs no build.
//!
//! ## Naming
//!
//! napi-rs renames every method to lowerCamelCase unless `js_name = "..."`
//! overrides it, so the check has to apply the same rule the generator does —
//! matching the Rust name would report a false miss on every multi-word
//! method, and matching case-insensitively would let a snake_case declaration
//! pass (the defect `the_emitted_declarations_are_not_snake_case` exists for).

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/<crate>")
        .to_path_buf()
}

/// The handwritten `#[napi]` modules. `generated.rs` is excluded on purpose:
/// it IS the emitter's output and the snapshot cross-reference already owns
/// it. `conformance.rs` is excluded because it is feature-gated test-only
/// surface that must NOT appear in the shipped `.d.ts` (F24).
const HANDWRITTEN: &[&str] = &[
    "crates/lunaris-ts/src/scope.rs",
    "crates/lunaris-ts/src/toggles.rs",
    "crates/lunaris-ts/src/open_overrides.rs",
    "crates/lunaris-ts/src/embedder_config.rs",
    "crates/lunaris-ts/src/reranker_config.rs",
    "crates/lunaris-ts/src/dsl.rs",
];

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Apply napi-rs's rename: `snake_case` -> `lowerCamelCase`.
fn lower_camel(rust: &str) -> String {
    let mut out = String::with_capacity(rust.len());
    let mut upper_next = false;
    for c in rust.chars() {
        if c == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// A `#[napi]` item's exported JS name, and the file it came from.
#[derive(Debug)]
struct Exported {
    js_name: String,
    file: &'static str,
    rust_name: String,
}

/// Scan one file for `#[napi(...)]`-attributed `pub fn` / `pub async fn`.
///
/// Deliberately line-oriented rather than a parse: the attribute and the
/// signature are adjacent by rustfmt, and a real parser here would be a
/// second thing to keep correct. Doc comments and other attributes between
/// the two are skipped.
fn exported_from(file: &'static str, src: &str) -> Vec<Exported> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim();
        if !t.starts_with("#[napi") {
            i += 1;
            continue;
        }
        // `js_name = "foo"` on this attribute wins over the derived name.
        let js_override = t.find("js_name").and_then(|k| {
            let rest = &t[k..];
            let a = rest.find('"')?;
            let b = rest[a + 1..].find('"')?;
            Some(rest[a + 1..a + 1 + b].to_string())
        });
        // A getter is a property, not a method — it appears in the .d.ts as
        // `get name(): T`, which the containment check below still finds.
        let mut j = i + 1;
        while j < lines.len() {
            let s = lines[j].trim();
            if s.starts_with("///") || s.starts_with("//") || s.starts_with("#[") || s.is_empty() {
                j += 1;
                continue;
            }
            break;
        }
        if let Some(s) = lines.get(j).map(|l| l.trim()) {
            let sig = s.strip_prefix("pub async fn ").or_else(|| s.strip_prefix("pub fn "));
            if let Some(sig) = sig
                && let Some(paren) = sig.find(['(', '<'])
            {
                let rust_name = sig[..paren].trim().to_string();
                if !rust_name.is_empty() {
                    out.push(Exported {
                        js_name: js_override.unwrap_or_else(|| lower_camel(&rust_name)),
                        file,
                        rust_name,
                    });
                }
            }
        }
        i = j.max(i + 1);
    }
    out
}

fn all_exported() -> Vec<Exported> {
    HANDWRITTEN.iter().flat_map(|f| exported_from(f, &read(f))).collect()
}

/// The check. Every handwritten `#[napi]` method must be declared in the
/// shipped `index.d.ts`.
#[test]
fn every_handwritten_napi_method_is_declared_in_the_shipped_dts() {
    let dts = read("crates/lunaris-ts/index.d.ts");
    // Tokenise so `retentionPolicy` cannot be satisfied by `retentionPolicyX`
    // or by a mention inside a doc comment's prose word.
    let declared: std::collections::HashSet<&str> = dts
        .lines()
        .filter(|l| !l.trim_start().starts_with('*') && !l.trim_start().starts_with("//"))
        .flat_map(|l| l.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')))
        .filter(|w| !w.is_empty())
        .collect();

    let missing: Vec<String> = all_exported()
        .into_iter()
        .filter(|e| !declared.contains(e.js_name.as_str()))
        .map(|e| format!("{}::{} (would export as `{}`)", e.file, e.rust_name, e.js_name))
        .collect();

    assert!(
        missing.is_empty(),
        "crates/lunaris-ts/index.d.ts is STALE — {} handwritten `#[napi]` method(s) exist \
         in Rust and are absent from the shipped type definitions, so no TypeScript caller \
         can reach them:\n  {}\n\nRegenerate and commit:\n  cd crates/lunaris-ts && npm ci \
         && npx napi build --platform --release\n\nDo NOT hand-edit index.d.ts: a \
         declaration that does not match what napi actually exports type-checks calls that \
         fail at runtime (F17).",
        missing.len(),
        missing.join("\n  ")
    );
}

/// Vacuity floor. The test above passes trivially on an empty scan — a
/// scanner that matched nothing would give a stale file a clean bill of
/// health, which is the fifth-instance-of-that-family failure the F3 entry
/// calls out by name.
#[test]
fn the_scanner_finds_a_realistic_handwritten_surface() {
    let found = all_exported();
    assert!(
        found.len() >= 25,
        "expected at least 25 handwritten `#[napi]` methods across {} files; the scanner \
         found {} ({:?}). Either the surface shrank drastically or the scanner stopped \
         matching the file shape — in which case the staleness test above asserts nothing.",
        HANDWRITTEN.len(),
        found.len(),
        found.iter().map(|e| &e.js_name).collect::<Vec<_>>()
    );
    // And it must reach EVERY listed file, not just the biggest one: a scan
    // that covers 5 of 6 looks exactly like a scan that covers 6 of 6.
    for f in HANDWRITTEN {
        assert!(
            found.iter().any(|e| e.file == *f),
            "the scanner found no `#[napi]` method in {f} — either it has none (drop it \
             from HANDWRITTEN and say why) or the scan does not reach it"
        );
    }
}

/// The rename rule must be the generator's, not an approximation.
#[test]
fn the_rename_matches_what_napi_does() {
    assert_eq!(lower_camel("retention_policy"), "retentionPolicy");
    assert_eq!(lower_camel("set_retention_policy"), "setRetentionPolicy");
    assert_eq!(lower_camel("url"), "url");
    assert_eq!(lower_camel("with_graph_pipeline"), "withGraphPipeline");
}

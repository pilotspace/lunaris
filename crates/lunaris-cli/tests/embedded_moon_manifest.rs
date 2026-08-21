//! `embedded-moon` must never enter a default feature set — CLAUDE.md, and now
//! `lunaris-cli` depends on it too.
//!
//! The invariant is a build-time one: the feature pulls in the whole Moon
//! server crate, so the day it lands in a `default = [...]` every
//! `cargo test --workspace` and every CI clippy run starts compiling a
//! database. Nothing in the source can catch that — only a manifest read can,
//! which is the same reasoning behind
//! `lunaris-core/tests/sdk_feature_forwarding.rs` and
//! `lunaris-hook/tests/embedded_moon_manifest.rs`.
//!
//! This file deliberately does NOT hard-code the list of crates the way the
//! hook's guard does. It walks `crates/*/Cargo.toml` and checks every manifest
//! that declares the feature, so the next crate to want an embedded Moon is
//! covered the moment it is created rather than the moment somebody remembers
//! to extend a list. (The hook's guard predates `lunaris-cli` and still names
//! three crates explicitly; when the two are merged, keep the walk.)

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Extract the `[...]` body of a `<feature> = [...]` declaration, spanning
/// multi-line arrays and skipping comment lines. Same shape as the hook guard's
/// parser — deliberately, so the two agree on what "in default" means.
fn feature_array(manifest_text: &str, feature: &str) -> Option<String> {
    let mut lines = manifest_text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix(feature) else {
            continue;
        };
        if !rest.trim_start().starts_with('=') {
            continue;
        }
        let open = line.find('[')?;
        let mut body = line[open + 1..].to_string();
        while !body.contains(']') {
            body.push_str(lines.next()?);
        }
        let close = body.rfind(']')?;
        body.truncate(close);
        return Some(body);
    }
    None
}

/// Every `crates/*/Cargo.toml` in the workspace, with its path relative to root.
fn all_crate_manifests() -> Vec<(String, String)> {
    let crates_dir = workspace_root().join("crates");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&crates_dir).expect("read crates/") {
        let path: PathBuf = entry.expect("dir entry").path();
        let manifest = path.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let name = path.file_name().expect("crate dir name").to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&manifest)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
        out.push((format!("crates/{name}/Cargo.toml"), text));
    }
    assert!(out.len() > 5, "the crate walk found almost nothing — did the layout move?");
    out
}

#[test]
fn embedded_moon_is_never_in_a_default_feature_set() {
    let mut checked = 0_usize;
    for (rel, text) in all_crate_manifests() {
        if feature_array(&text, "embedded-moon").is_none() {
            continue;
        }
        checked += 1;
        let Some(default) = feature_array(&text, "default") else {
            continue;
        };
        assert!(
            !default.contains("embedded-moon"),
            "{rel}: `embedded-moon` is inside `default = [{default}]`. It pulls in the \
             Moon server crate, so every `cargo test --workspace` and every CI clippy \
             run would compile a database. It must stay opt-in: the published release \
             artifact turns it on explicitly, nothing else does."
        );
    }
    assert!(
        checked >= 3,
        "expected at least lunaris-cli, lunaris-hook, lunaris-mcp and \
         lunaris-memory-service to declare `embedded-moon`; found {checked}. If the \
         feature was renamed, this guard is now checking nothing."
    );
}

/// `lunaris-cli` specifically — the crate this test file belongs to. Named on
/// its own so a failure points at the right manifest instead of a loop index.
#[test]
fn the_cli_declares_embedded_moon_opt_in_and_forwards_to_the_shared_launcher() {
    let path: &Path = &workspace_root().join("crates/lunaris-cli/Cargo.toml");
    let text = std::fs::read_to_string(path).expect("read the lunaris-cli manifest");

    let default = feature_array(&text, "default").expect("lunaris-cli must declare `default`");
    assert!(
        !default.contains("embedded-moon"),
        "lunaris-cli: `embedded-moon` must not be in `default = [{default}]`"
    );

    let array = feature_array(&text, "embedded-moon")
        .expect("lunaris-cli must declare an `embedded-moon` feature for `lunaris try`");
    assert!(
        array.contains("\"lunaris-memory-service/embedded-moon\""),
        "lunaris-cli: `embedded-moon` must forward to the ONE launcher definition in \
         lunaris-memory-service, got: [{array}]"
    );
    assert!(
        !array.contains("moon_server"),
        "lunaris-cli: `embedded-moon` grew a private `dep:moon_server`. That is how \
         three divergent launchers appear; forward to lunaris-memory-service instead. \
         Got: [{array}]"
    );
}

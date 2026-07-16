//! Manifest guards for the `embedded-moon` feature (contextd unification).
//!
//! Two invariants, both CLAUDE.md-level:
//! 1. `embedded-moon` is NEVER inside `default = [...]` in ANY crate that
//!    declares it — `cargo test --workspace` / CI clippy must not compile the
//!    Moon server.
//! 2. `lunaris-hook` and `lunaris-mcp` FORWARD to the single launcher
//!    definition (`lunaris-memory-service/embedded-moon`) instead of growing
//!    private `dep:moon_server` copies — the promotion that unified the
//!    contextd + mcp processors must not silently regress into three
//!    divergent launchers.
//!
//! Same manifest-parsing approach as
//! `lunaris-core/tests/sdk_feature_forwarding.rs`: unit tests behind features
//! cannot catch a manifest regression, only a manifest read can.

use std::path::PathBuf;

fn manifest(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Extract the `[...]` array body of a feature declaration, skipping comment
/// lines (same approach as sdk_feature_forwarding.rs::feature_array, extended
/// to span multi-line arrays).
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

const DECLARING_MANIFESTS: &[&str] = &[
    "crates/lunaris-hook/Cargo.toml",
    "crates/lunaris-mcp/Cargo.toml",
    "crates/lunaris-memory-service/Cargo.toml",
];

#[test]
fn embedded_moon_never_in_default_features() {
    for rel in DECLARING_MANIFESTS {
        let text = manifest(rel);
        assert!(
            feature_array(&text, "embedded-moon").is_some(),
            "{rel}: expected an `embedded-moon` feature declaration"
        );
        if let Some(default) = feature_array(&text, "default") {
            assert!(
                !default.contains("embedded-moon"),
                "{rel}: `embedded-moon` must NEVER be in `default = [...]` \
                 (CLAUDE.md invariant — workspace builds must not compile the moon server), \
                 got: [{default}]"
            );
        }
    }
}

#[test]
fn hook_and_mcp_forward_to_the_single_launcher() {
    for rel in ["crates/lunaris-hook/Cargo.toml", "crates/lunaris-mcp/Cargo.toml"] {
        let text = manifest(rel);
        let array = feature_array(&text, "embedded-moon")
            .unwrap_or_else(|| panic!("{rel}: missing `embedded-moon` feature"));
        assert!(
            array.contains("\"lunaris-memory-service/embedded-moon\""),
            "{rel}: `embedded-moon` must forward to lunaris-memory-service/embedded-moon \
             (the ONE launcher definition), got: [{array}]"
        );
        assert!(
            !array.contains("dep:moon_server"),
            "{rel}: `embedded-moon` must not re-grow a private moon_server dep \
             — the launcher was promoted to lunaris-memory-service, got: [{array}]"
        );
    }
}

#[test]
fn memory_service_owns_the_moon_server_dep() {
    let text = manifest("crates/lunaris-memory-service/Cargo.toml");
    let array = feature_array(&text, "embedded-moon")
        .expect("lunaris-memory-service: missing `embedded-moon` feature");
    assert!(
        array.contains("dep:moon_server"),
        "lunaris-memory-service: `embedded-moon` must gate the moon_server dep, got: [{array}]"
    );
}

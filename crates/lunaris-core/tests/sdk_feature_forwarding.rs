//! SDK feature-forwarding guard (found 2026-07-16, moon-v080-bump G6 triage).
//!
//! Both SDK cdylibs import the umbrella crate with `default-features =
//! false`, so their `llamacpp` feature MUST explicitly forward
//! `"lunaris/llamacpp"` (and each GPU feature its `lunaris/<gpu>` twin).
//! Without the forward, a standalone `maturin build` / `napi build`
//! resolves the umbrella WITHOUT `llamacpp`, and `Lunaris::open()` falls
//! back to `NoopEmbedder` — every vector is zeros, hybrid recall silently
//! degrades to BM25 + insertion-order tie-breaks. Workspace builds mask
//! the bug via feature unification (another member enables the feature),
//! which is why `cargo test --workspace` alone can never catch it.

use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/crates/lunaris-core; workspace root is ../..
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // workspace root
    p
}

/// Extract the array body of `<feature> = [ ... ]` from a Cargo.toml
/// `[features]` section. Comment lines are stripped so a commented-out
/// forward cannot satisfy the guard.
fn feature_array(manifest: &str, feature: &str) -> Option<String> {
    let uncommented: String = manifest
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    // Match `<feature>` at line start followed by any spacing and `=`
    // (manifests align with extra spaces, e.g. `cuda  = [...]`).
    let start = uncommented.lines().find_map(|l| {
        let t = l.trim_start();
        let rest = t.strip_prefix(feature)?;
        if rest.trim_start().starts_with('=') {
            let off = l.as_ptr() as usize - uncommented.as_ptr() as usize;
            Some(off)
        } else {
            None
        }
    })?;
    let rest = &uncommented[start..];
    let open = rest.find('[')?;
    let close = rest[open..].find(']')? + open;
    Some(rest[open..=close].to_string())
}

fn assert_forwards(sdk_manifest_rel: &str, feature: &str, forward: &str) {
    let path = workspace_root().join(sdk_manifest_rel);
    let manifest =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let array = feature_array(&manifest, feature)
        .unwrap_or_else(|| panic!("{sdk_manifest_rel}: feature `{feature}` not found"));
    assert!(
        array.contains(&format!("\"{forward}\"")),
        "{sdk_manifest_rel}: feature `{feature}` must forward \"{forward}\" to the \
         umbrella crate (imported with default-features = false). Without it a \
         standalone SDK build silently resolves NoopEmbedder → zero vectors. \
         Current array: {array}"
    );
}

#[test]
fn py_sdk_llamacpp_forwards_to_umbrella() {
    assert_forwards("crates/lunaris-py/Cargo.toml", "llamacpp", "lunaris/llamacpp");
}

#[test]
fn ts_sdk_llamacpp_forwards_to_umbrella() {
    assert_forwards("crates/lunaris-ts/Cargo.toml", "llamacpp", "lunaris/llamacpp");
}

#[test]
fn py_sdk_gpu_features_forward_to_umbrella() {
    for gpu in ["metal", "cuda", "vulkan"] {
        assert_forwards("crates/lunaris-py/Cargo.toml", gpu, &format!("lunaris/{gpu}"));
    }
}

#[test]
fn ts_sdk_gpu_features_forward_to_umbrella() {
    for gpu in ["metal", "cuda", "vulkan"] {
        assert_forwards("crates/lunaris-ts/Cargo.toml", gpu, &format!("lunaris/{gpu}"));
    }
}

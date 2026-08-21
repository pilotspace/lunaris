//! Feature-forwarding guard for every crate that imports the umbrella with
//! `default-features = false` (found 2026-07-16, moon-v080-bump G6 triage;
//! widened to `lunaris-bench` in 0.6.2 when the version-controlled LME
//! harness landed).
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

/// `lunaris-bench` hosts the `lunaris-evals` binary that the committed
/// LongMemEval harness (`scripts/bench/lme/`) drives, and it imports the
/// umbrella with `default-features = false` like the SDKs do.
///
/// The harness's default embedder lane is a warm remote Ollama server
/// (`LUNARIS_EMBEDDER_OLLAMA_URL`) — that is how the N=125 A/B dodges the
/// llama.cpp Metal-contention deadlock under one-process-per-question. That
/// lane only exists if `resolve_embedder`'s `#[cfg(feature = "embed-remote")]`
/// arm is compiled in, which requires the forward below. Without it the eval
/// binary silently resolves `NoopEmbedder` (zero vectors) and the whole run
/// measures BM25 + insertion-order tie-breaks while still printing a J-score.
#[test]
fn bench_embed_remote_forwards_to_umbrella() {
    assert_forwards("crates/lunaris-bench/Cargo.toml", "embed-remote", "lunaris/embed-remote");
}

/// The in-process lane of the same harness. Already green — pinned so a
/// future edit cannot quietly drop it (727bc65 was exactly this regression).
#[test]
fn bench_llamacpp_and_gpu_features_forward_to_umbrella() {
    assert_forwards("crates/lunaris-bench/Cargo.toml", "llamacpp", "lunaris/llamacpp");
    for gpu in ["metal", "cuda", "vulkan"] {
        assert_forwards("crates/lunaris-bench/Cargo.toml", gpu, &format!("lunaris/{gpu}"));
    }
}

/// W1.2 — `lunaris-server` was the surface this guard did not cover, and it
/// was the surface that shipped the bug. It had NO `[features]` block at all,
/// so `cargo build -p lunaris-server` resolved the umbrella with
/// `default-features = false` (the workspace entry), produced a `NoopEmbedder`,
/// and `/readyz` still reported green. Every other binary surface hard-wires
/// `features = ["llamacpp"]` on its dependency line
/// (`lunaris-mcp`, `lunaris-hook`, `lunaris-cli`, `lunaris-memory-service`);
/// the server routes it through a default feature instead so
/// `--no-default-features` stays a deliberate, greppable Tier-0 opt-out.
#[test]
fn server_llamacpp_and_gpu_features_forward_to_umbrella() {
    assert_forwards("crates/lunaris-server/Cargo.toml", "llamacpp", "lunaris/llamacpp");
    for gpu in ["metal", "cuda", "vulkan"] {
        assert_forwards("crates/lunaris-server/Cargo.toml", gpu, &format!("lunaris/{gpu}"));
    }
}

/// The forwarding above is necessary but not sufficient: a `llamacpp` feature
/// nobody enables is the same zero-vector build. A plain
/// `cargo build -p lunaris-server` must land on the real embedder, so
/// `llamacpp` has to be in the server's `default` set.
#[test]
fn server_default_features_include_llamacpp() {
    let path = workspace_root().join("crates/lunaris-server/Cargo.toml");
    let manifest =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let default = feature_array(&manifest, "default")
        .expect("crates/lunaris-server/Cargo.toml: no `default` feature — a plain `cargo build -p lunaris-server` would resolve NoopEmbedder");
    assert!(
        default.contains("\"llamacpp\""),
        "crates/lunaris-server/Cargo.toml: `default` must include \"llamacpp\" so a plain \
         `cargo build -p lunaris-server` cannot silently produce a zero-vector embedder. \
         Current array: {default}"
    );
}

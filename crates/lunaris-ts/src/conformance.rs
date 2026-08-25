//! NOT codegen-managed — conformance-only helpers (Plan 08-04).
//!
//! Exposed only when the `bindings-it` feature is enabled. Production npm
//! packages NEVER ship this module. The `lunaris-codegen` parity-check
//! walker explicitly excludes any file named `conformance.rs` via the
//! explicit three-path enumeration in `codegen_managed_paths` (Plan 08-01
//! T-08-01-05 carveout).
//!
//! ## Surface (revision iteration 2 — trimmed)
//!
//! - `conformanceFixtureEpisodes(seed, count) -> Array<object>` — returns
//!   `FixtureCorpus::episodes()` serialised to plain JS objects. `seed`
//!   and `count` arguments are asserted against the canonical Rust
//!   constants so silent cross-language drift surfaces as a hard
//!   error at the FFI boundary.
//! - `Lunaris.scanKvPrefix(prefix: Buffer) -> Promise<Array<[Buffer, Buffer]>>`
//!   — thin wrapper over `StoragePort::scan_range(prefix, None) →
//!   futures::TryStreamExt::try_collect`. Lets Plan 08-04 drivers dump
//!   every KV row under a prefix for the Moon-vs-Postgres parity check.
//!
//! Earlier revisions also exposed `withZeroEmbedder` and `withClockSeed`.
//! BOTH REMOVED in revision iteration 2 — Plan 08-04's per-driver
//! backend parity test does NOT require embedder or clock determinism
//! (a single driver process writes identical bytes to both backends via
//! `StoragePort::atomic_write`). Keeping the surface minimal also keeps
//! the `lunaris-core` entropy sources (`primitives::Ulid::new()`,
//! `hlc.rs`) untouched.

#![cfg(feature = "bindings-it")]

use napi::bindgen_prelude::*;
use napi_derive::napi;

use ::lunaris_conformance::fixtures::FixtureCorpus;

use crate::Lunaris;
use crate::errors::napi_err;

/// Returns an array of objects mirroring `FixtureCorpus::episodes()`.
///
/// `seed` and `count` MUST match `lunaris_conformance::fixtures::SEED` /
/// `EPISODE_COUNT` — we accept them as args so drivers can assert the
/// constants at the FFI boundary, but the underlying `FixtureCorpus::new()`
/// always uses the canonical Rust constants. Drift surfaces as a hard
/// napi::Error with the canonical Rust value in the message so operators
/// know which side of the FFI needs updating.
///
/// `seed` is a `BigInt` because TypeScript's `number` loses precision
/// above 2^53; the `0xCAFE_F00D` seed is representable as `number` but
/// the API intentionally uses `BigInt` to future-proof against a larger
/// seed.
#[napi(js_name = "conformanceFixtureEpisodes")]
pub fn conformance_fixture_episodes(seed: BigInt, count: u32) -> napi::Result<serde_json::Value> {
    let (signed, seed_u64, lossless) = seed.get_u64();
    if signed {
        return Err(napi::Error::new(Status::InvalidArg, "seed must be non-negative".to_string()));
    }
    if !lossless {
        return Err(napi::Error::new(Status::InvalidArg, "seed exceeds u64 range".to_string()));
    }
    if seed_u64 != ::lunaris_conformance::fixtures::SEED {
        return Err(napi::Error::new(
            Status::InvalidArg,
            format!(
                "conformance seed drift — Rust constant is 0x{:x}",
                ::lunaris_conformance::fixtures::SEED
            ),
        ));
    }
    if (count as usize) != ::lunaris_conformance::fixtures::EPISODE_COUNT {
        return Err(napi::Error::new(
            Status::InvalidArg,
            format!(
                "conformance episode-count drift — Rust constant is {}",
                ::lunaris_conformance::fixtures::EPISODE_COUNT
            ),
        ));
    }
    let corpus = FixtureCorpus::new();
    let arr: Vec<serde_json::Value> = corpus
        .episodes()
        .iter()
        .map(|ep| serde_json::to_value(ep).expect("Episode Serialize infallible"))
        .collect();
    Ok(serde_json::Value::Array(arr))
}

/// Additional free `#[napi]` function that takes the `Lunaris` handle by
/// reference and dumps every row under the given prefix.
///
/// Returns `Array<[Buffer, Buffer]>` — one 2-element array per KV row.
/// napi-rs 3.x maps `Vec<Vec<Buffer>>` to exactly that shape at the TS
/// layer.
///
/// Plan 08-04's driver dumps every row under the fixture-corpus write
/// prefixes from Moon and Postgres separately, then asserts the two
/// dumps are structurally-equal against a committed golden reference.
/// Exposed as a free function rather than a method on `Lunaris` because
/// a second `#[napi] impl Lunaris` block would introduce duplicate-symbol
/// risk with the generated `impl` in `generated.rs`.
#[napi(js_name = "scanKvPrefix")]
pub async fn scan_kv_prefix(handle: &Lunaris, prefix: Buffer) -> napi::Result<Vec<Vec<Buffer>>> {
    use futures::TryStreamExt;
    let storage = handle.inner.storage();
    let prefix_vec: Vec<u8> = prefix.to_vec();
    let scope = ::lunaris_core::scope::Scope::dev();
    let stream = storage
        .scan_range(&scope, &prefix_vec, None)
        .await
        .map_err(|e| napi_err(::lunaris::LunarisError::Storage(e)))?;
    let rows: Vec<(bytes::Bytes, bytes::Bytes)> =
        stream.try_collect().await.map_err(|e| napi_err(::lunaris::LunarisError::Storage(e)))?;
    Ok(rows
        .into_iter()
        .map(|(k, v)| vec![Buffer::from(k.to_vec()), Buffer::from(v.to_vec())])
        .collect())
}

/// Plan 21-03 — cross-SDK embedder parity probe. Returns raw f32 vectors
/// from the wrapped `Arc<dyn Embedder>` inside an `EmbedderConfig`.
/// Feature-gated; release npm builds strip it.
#[napi(js_name = "embedderConfigEmbedBatch")]
pub async fn embedder_config_embed_batch(
    cfg: &crate::embedder_config::EmbedderConfig,
    inputs: Vec<String>,
) -> napi::Result<Vec<Vec<f64>>> {
    let embedder = cfg.inner.clone();
    let refs: Vec<&str> = inputs.iter().map(|s| s.as_str()).collect();
    let out = embedder.embed_batch(&refs).await.map_err(napi_err)?;
    Ok(out.into_iter().map(|row| row.into_iter().map(|x| x as f64).collect()).collect())
}

/// W4.10 — resolve the PROCESS-DEFAULT embedder (the same GGUF → remote →
/// Noop chain `Lunaris::open` walks) and hand it back as an
/// `EmbedderConfig`.
///
/// `EmbedderConfig` exposes no remote factory, so a parity probe limited to
/// `llamacpp()` can only run where a GGUF is staged — never on a CI runner.
/// It would report "skipped" forever, and a permanent skip is
/// indistinguishable from a passing check. Routing through the production
/// resolver lets one probe cover llama.cpp locally and the stub OpenAI
/// embedder in CI.
///
/// A `NoopEmbedder` resolution is NOT reported here; the caller detects it
/// behaviourally (all-zero vectors), so this stays policy-free.
#[napi(js_name = "embedderConfigFromEnv")]
pub async fn embedder_config_from_env() -> napi::Result<crate::embedder_config::EmbedderConfig> {
    let inner = ::lunaris::resolve_default_embedder().await.map_err(napi_err)?;
    let declared_dim = inner.dim() as u32;
    Ok(crate::embedder_config::EmbedderConfig { inner, declared_dim })
}

#[cfg(test)]
mod tests {
    // Unit-level check that the handwritten Rust side compiles and that
    // FixtureCorpus is reachable under the feature gate. Full TypeScript
    // integration (via conformanceFixtureEpisodes) is exercised by
    // Plan 08-04's backend-parity driver.

    #[test]
    fn test_conformance_helpers_exist() {
        let corpus = ::lunaris_conformance::fixtures::FixtureCorpus::new();
        assert_eq!(corpus.episodes().len(), ::lunaris_conformance::fixtures::EPISODE_COUNT);
    }
}

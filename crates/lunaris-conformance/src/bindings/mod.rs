//! Plan 08-04 — per-driver backend parity.
//!
//! Each language (Rust / Python / TypeScript) drives a round-trip via
//! its own `Lunaris::open` + handle ingest pipeline into Moon. The name
//! predates 0.7.0, when the same corpus went into BOTH Moon and Postgres in
//! one process and the claim was that `StoragePort::atomic_write` fans
//! identical bytes to each. With one backend left, what is still asserted —
//! and what actually caught regressions — is that all THREE language drivers
//! produce the same structural row set against the same committed golden.
//!
//! This module provides:
//!
//! - [`GoldenReference`] — loads `fixtures/golden/bindings_fixture.json`.
//! - [`NormalizedRows`] — the structural shape compared across runs.
//! - [`collect_normalized_chunk_rows`] — scans a handle and normalizes.
//! - [`assert_structural_eq`] — gate used by the Rust test; the Python
//!   and TypeScript drivers replicate the same shape in their own
//!   languages and assert against the same committed golden JSON.
//! - [`run_rust_driver_backend_parity`] — the Rust-side test entry.
//!   Opens a Moon handle (controlled by an env var) and asserts its row
//!   set matches the golden reference. It took a Postgres handle too
//!   through 0.6.x; that arm went with the backend in 0.7.0.
//!
//! NO byte-exact HLC / ULID comparison — those vary per run.
//! Normalization strips them so we test structural invariants only
//! (row count, prefix coverage, distinct episode ID count).
//!
//! ## Scope-reset guard (revision iteration 2)
//!
//! This module must NOT make any cross-language assertion like
//! `assert_eq!(rust_rows, py_rows)`. Each language driver is tested
//! INDEPENDENTLY against the committed golden reference. Revision
//! iteration 1 of Plan 08-04 introduced a 3-way cross-language
//! byte-identity assertion that required a clock seam + a
//! deterministic `ZeroEmbedder`; both were deleted in revision
//! iteration 2 because the ROADMAP success criterion's "AND" joins
//! BACKENDS within a single driver run, not LANGUAGES.
//!
//! See `.planning/phases/08-sdk-bindings/08-04-PLAN.md` (revision 2)
//! and `.planning/phases/08-sdk-bindings/08-CONTEXT.md` §decisions for
//! the full rationale.

#![cfg(feature = "bindings-it")]

use std::sync::Arc;

use anyhow::{Context, bail};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};

use ::lunaris::Lunaris;

/// Golden reference — committed at
/// `crates/lunaris-conformance/fixtures/golden/bindings_fixture.json`.
///
/// Each field records a STRUCTURAL invariant of the Plan 05-02
/// `FixtureCorpus` ingest. Not byte-exact HLC stamps / ULIDs — those
/// vary per run. The driver tests assert:
///
/// - `scan_range("episode:conformance:", None)` returns exactly
///   `episode_count * per_episode_key_count` rows.
/// - Distinct episode IDs (ULIDs stripped from the key suffix) == `episode_count`.
/// - Prefix matches `keys_prefix`.
///
/// `chunk_count_per_episode` and `embedding_dim` are documentary
/// fields reserved for future test extensions (e.g., chunking the
/// FixtureCorpus into 3 chunks per episode, or encoding embeddings
/// into the payload). Not asserted in revision 2.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GoldenReference {
    /// Number of distinct episodes the fixture corpus emits.
    /// Must match `::lunaris_conformance::fixtures::EPISODE_COUNT`.
    pub episode_count: usize,
    /// Number of chunks per episode (documentary — not asserted).
    pub chunk_count_per_episode: usize,
    /// Key prefix for every ingested row. Must match
    /// `build_episode_ops` in `src/fixtures/mod.rs` which writes
    /// `format!("episode:conformance:{}", ep.id)`.
    pub keys_prefix: String,
    /// Embedding dimension (documentary — not asserted).
    pub embedding_dim: usize,
    /// Number of KV rows written per episode. `build_episode_ops`
    /// writes ONE `WriteOp::KvPut` per episode, so this is 1.
    pub per_episode_key_count: usize,
}

impl GoldenReference {
    /// Load the committed golden reference from the source tree.
    ///
    /// Uses `CARGO_MANIFEST_DIR` so the test works from any working
    /// directory.
    pub fn load() -> anyhow::Result<Self> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/golden/bindings_fixture.json");
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("load golden at {}", path.display()))?;
        Ok(serde_json::from_str(&raw)?)
    }
}

/// Structural shape of the scan output — strips HLC + ULID values so
/// two runs (or two backends of the same run) compare equal even when
/// wall_ms drift and fresh ULIDs differ.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NormalizedRows {
    /// Total number of `(key, value)` pairs returned by
    /// `scan_range(prefix, None)`.
    pub total_rows: usize,
    /// The prefix the scan was called with. Echoed back into the
    /// normalized shape so drift surfaces a readable error.
    pub keys_prefix: String,
    /// Number of distinct episode IDs — each row's key is parsed as
    /// `"{prefix}{ulid}"` and the suffix is collected into a
    /// `BTreeSet` to count unique values.
    pub distinct_episode_ids: usize,
}

/// Scan a handle at the given prefix and normalize into [`NormalizedRows`].
///
/// Any driver (Rust direct, or Python / TypeScript via their
/// `scan_kv_prefix` / `scanKvPrefix` helpers) can produce this shape
/// from their scan output. The structural assert ([`assert_structural_eq`])
/// operates on the shape, not the raw rows.
pub async fn collect_normalized_chunk_rows(
    handle: &Lunaris,
    keys_prefix: &str,
) -> anyhow::Result<NormalizedRows> {
    let storage = handle.storage();
    let stream =
        storage.scan_range(&lunaris_core::Scope::dev(), keys_prefix.as_bytes(), None).await?;
    let rows: Vec<(bytes::Bytes, bytes::Bytes)> = stream.try_collect().await?;

    // Distinct episode IDs: parse the key as "{prefix}{ulid}" and
    // count unique ULIDs.
    let distinct_episode_ids = rows
        .iter()
        .filter_map(|(k, _)| std::str::from_utf8(k).ok())
        .filter_map(|s| s.strip_prefix(keys_prefix))
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    Ok(NormalizedRows {
        total_rows: rows.len(),
        keys_prefix: keys_prefix.to_string(),
        distinct_episode_ids,
    })
}

/// Gate that the Rust test calls; Py + TS drivers replicate the same
/// structural comparison in their own languages against the same
/// committed golden JSON.
///
/// Returns `Err(anyhow)` with a human-readable reason when any
/// structural invariant drifts. The error message always includes
/// BOTH the observed and the expected values so operators can
/// diagnose without re-running.
pub fn assert_structural_eq(rows: &NormalizedRows, golden: &GoldenReference) -> anyhow::Result<()> {
    if rows.keys_prefix != golden.keys_prefix {
        bail!("prefix drift: got {:?} want {:?}", rows.keys_prefix, golden.keys_prefix);
    }
    if rows.distinct_episode_ids != golden.episode_count {
        bail!(
            "episode count drift: got {} want {}",
            rows.distinct_episode_ids,
            golden.episode_count
        );
    }
    let want_total = golden.episode_count * golden.per_episode_key_count;
    if rows.total_rows != want_total {
        bail!(
            "row-count drift: got {} want {} ({} episodes × {} keys/episode)",
            rows.total_rows,
            want_total,
            golden.episode_count,
            golden.per_episode_key_count
        );
    }
    Ok(())
}

/// Rust-driver backend-parity test body.
///
/// The caller reads `LUNARIS_MOON_URL` from the environment. When it is unset
/// the test exits `Ok(())` without exercising any code path (the two-tier skip
/// pattern from `tests/run_storage_moon.rs`) — the harness is still validated
/// at the type level by `cargo check`. `conformance-bindings.yml` builds and
/// launches a Moon and sets the var, so CI takes the full path rather than the
/// skip.
///
/// Per-backend flow:
///
/// 1. Open a `Lunaris` handle against the URL.
/// 2. Ingest the FixtureCorpus (10 deterministic episodes) via
///    `FixtureCorpus::ingest_into(handle.storage())` — ONE
///    `atomic_write` per episode, per INGEST-04 invariant.
/// 3. `scan_range(b"episode:conformance:", None)` + `try_collect`.
/// 4. Normalize into [`NormalizedRows`].
/// 5. [`assert_structural_eq`] against the golden reference.
///
/// Critically: the backend is compared AGAINST THE GOLDEN, not against
/// another driver. No cross-language byte-identity assertion is made
/// anywhere in this function.
///
/// 0.7.0 dropped the `postgres_url` arm with `lunaris-storage-postgres`. The
/// "backend parity" in the name is now parity between the three LANGUAGE
/// drivers' independent runs against the same golden, over one substrate.
pub async fn run_rust_driver_backend_parity(moon_url: Option<&str>) -> anyhow::Result<()> {
    let golden = GoldenReference::load()?;

    let Some(url) = moon_url else {
        eprintln!("run_bindings_backend_parity: SKIP (LUNARIS_MOON_URL unset)");
        return Ok(());
    };

    let handle =
        Lunaris::open(url).await.with_context(|| format!("Lunaris::open({url}) for Moon backend"))?;
    exercise_one_backend(&handle, &golden, "moon").await
}

/// Per-backend ingest + scan + normalize + assert.
///
/// Factored out so the caller's orchestration loop stays compact and the error
/// surface names the store in the context (`"moon backend failed structural
/// parity"`). Only one label is passed today; the parameter stays so adding a
/// backend does not mean re-threading the error context.
async fn exercise_one_backend(
    handle: &Lunaris,
    golden: &GoldenReference,
    backend_label: &str,
) -> anyhow::Result<()> {
    let corpus = crate::fixtures::FixtureCorpus::new();
    let storage: Arc<dyn lunaris_core::StoragePort> = handle.storage();
    corpus
        .ingest_into(&storage)
        .await
        .with_context(|| format!("ingest FixtureCorpus into {backend_label}"))?;
    let rows = collect_normalized_chunk_rows(handle, &golden.keys_prefix)
        .await
        .with_context(|| format!("scan_range on {backend_label}"))?;
    assert_structural_eq(&rows, golden)
        .with_context(|| format!("{backend_label} backend failed structural parity"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_loads_from_committed_json() {
        let g = GoldenReference::load().expect("golden reference must load");
        // Sanity: values match the canonical Plan 05-02 fixture
        // corpus (`EPISODE_COUNT = 10`, `build_episode_ops` writes ONE
        // `WriteOp::KvPut` per episode under the
        // `episode:conformance:` prefix).
        assert_eq!(g.episode_count, crate::fixtures::EPISODE_COUNT);
        assert_eq!(g.keys_prefix, "episode:conformance:");
        assert_eq!(g.per_episode_key_count, 1);
    }

    #[test]
    fn assert_structural_eq_detects_row_count_drift() {
        let golden = GoldenReference {
            episode_count: 10,
            chunk_count_per_episode: 3,
            keys_prefix: "episode:conformance:".to_string(),
            embedding_dim: 128,
            per_episode_key_count: 1,
        };
        let rows = NormalizedRows {
            total_rows: 9, // one short
            keys_prefix: "episode:conformance:".to_string(),
            distinct_episode_ids: 10,
        };
        let err = assert_structural_eq(&rows, &golden).unwrap_err();
        assert!(format!("{err}").contains("row-count drift"));
    }

    #[test]
    fn assert_structural_eq_detects_prefix_drift() {
        let golden = GoldenReference {
            episode_count: 10,
            chunk_count_per_episode: 3,
            keys_prefix: "episode:conformance:".to_string(),
            embedding_dim: 128,
            per_episode_key_count: 1,
        };
        let rows = NormalizedRows {
            total_rows: 10,
            keys_prefix: "chunk:".to_string(),
            distinct_episode_ids: 10,
        };
        let err = assert_structural_eq(&rows, &golden).unwrap_err();
        assert!(format!("{err}").contains("prefix drift"));
    }

    #[test]
    fn assert_structural_eq_detects_episode_drift() {
        let golden = GoldenReference {
            episode_count: 10,
            chunk_count_per_episode: 3,
            keys_prefix: "episode:conformance:".to_string(),
            embedding_dim: 128,
            per_episode_key_count: 1,
        };
        let rows = NormalizedRows {
            total_rows: 10,
            keys_prefix: "episode:conformance:".to_string(),
            distinct_episode_ids: 8, // two duplicate keys
        };
        let err = assert_structural_eq(&rows, &golden).unwrap_err();
        assert!(format!("{err}").contains("episode count drift"));
    }

    #[test]
    fn assert_structural_eq_accepts_matching_shape() {
        let golden = GoldenReference {
            episode_count: 10,
            chunk_count_per_episode: 3,
            keys_prefix: "episode:conformance:".to_string(),
            embedding_dim: 128,
            per_episode_key_count: 1,
        };
        let rows = NormalizedRows {
            total_rows: 10,
            keys_prefix: "episode:conformance:".to_string(),
            distinct_episode_ids: 10,
        };
        assert!(assert_structural_eq(&rows, &golden).is_ok());
    }
}

//! Plan 08-04 — per-driver backend parity.
//!
//! Each language (Rust / Python / TypeScript) drives a round-trip via
//! its own `Lunaris::open` + handle ingest pipeline into BOTH Moon and
//! Postgres within a single process. Within a process, `StoragePort::
//! atomic_write` fans identical bytes to each backend (proven at the
//! v0.1.0 Phase 1 level), so Moon rows and Postgres rows are
//! structurally identical by construction.
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
//!   Opens Moon + Postgres handles (controlled by env vars) and
//!   asserts each backend's row set matches the golden reference.
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
    let stream = storage.scan_range(keys_prefix.as_bytes(), None).await?;
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
/// Reads `LUNARIS_MOON_URL` + `LUNARIS_POSTGRES_URL` from the
/// environment. When either is unset, skips the corresponding backend
/// with `Ok(())` (follows the Plan 04-03 / 05-02 two-tier skip
/// pattern from `tests/run_storage_moon.rs` + `run_storage_postgres.rs`).
/// When BOTH are unset, the test exits `Ok(())` without exercising
/// any code path — the harness is still validated at the type level
/// by `cargo check`. The Plan 08-04 CI job sets both env vars so at
/// least the `memory` matrix row (mapped to `postgres://` in CI) and
/// the `postgres` row exercise the full path; the `moon` matrix row
/// skips neutrally when Moon is not configured.
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
/// Critically: the two backends are compared AGAINST THE GOLDEN, not
/// against each other. No cross-language byte-identity assertion is
/// made anywhere in this function.
pub async fn run_rust_driver_backend_parity(
    moon_url: Option<&str>,
    postgres_url: Option<&str>,
) -> anyhow::Result<()> {
    let golden = GoldenReference::load()?;

    if moon_url.is_none() && postgres_url.is_none() {
        eprintln!(
            "run_bindings_backend_parity: SKIP (LUNARIS_MOON_URL and LUNARIS_POSTGRES_URL both unset)"
        );
        return Ok(());
    }

    if let Some(url) = moon_url {
        let handle = Lunaris::open(url)
            .await
            .with_context(|| format!("Lunaris::open({url}) for Moon backend"))?;
        exercise_one_backend(&handle, &golden, "moon").await?;
    } else {
        eprintln!("run_bindings_backend_parity: SKIP Moon (LUNARIS_MOON_URL unset)");
    }

    if let Some(url) = postgres_url {
        let handle = Lunaris::open(url)
            .await
            .with_context(|| format!("Lunaris::open({url}) for Postgres backend"))?;
        exercise_one_backend(&handle, &golden, "postgres").await?;
    } else {
        eprintln!("run_bindings_backend_parity: SKIP Postgres (LUNARIS_POSTGRES_URL unset)");
    }

    Ok(())
}

/// Per-backend ingest + scan + normalize + assert.
///
/// Factored out so the caller's orchestration loop stays compact and
/// each backend's error surface names the backend in the context
/// (`"Moon backend failed structural parity"` vs `"Postgres backend
/// failed structural parity"`).
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

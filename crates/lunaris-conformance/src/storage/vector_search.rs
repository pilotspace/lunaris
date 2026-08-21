//! Plan 05-02 D-17 — `vector_search` conformance.
//!
//! Seeds 3 chunk vectors via `fixtures::seed_three_chunks` (B-3 fix —
//! defined in Task 1, called unconditionally here), queries with the
//! exact target vector, and asserts at least one hit comes back.
//! `index="chunks"` is the canonical chunk index name every backend
//! whitelists.

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::storage::StoragePort;

use crate::suite_scope::SuiteScope;

/// Vector dimensionality for the conformance fixture. Must match Moon's
/// `ensure_indexes` (`DIM = 768`) — any other value triggers a backend-side
/// dim-mismatch error (debug session `conformance-dim-mismatch.md`,
/// 2026-04-24). Centralised in `fixtures::EMBED_DIM`.
const FIXTURE_DIM: usize = crate::fixtures::EMBED_DIM;

pub async fn recall(storage: &Arc<dyn StoragePort>, run: &SuiteScope) -> anyhow::Result<()> {
    // Predictable target vector: small deterministic ramp over the full
    // 768d space (`i * 0.001` keeps values bounded). The fixture helper
    // seeds index-0 with this exact vector and indices 1+2 with perturbed
    // copies, so a query for `target_vec` MUST rank the index-0 chunk
    // first when the backend honours cosine distance.
    let target_vec: Vec<f32> = (0..FIXTURE_DIM).map(|i| (i as f32) * 0.001).collect();

    let written = crate::fixtures::seed_three_chunks(storage, run, &target_vec).await?;
    anyhow::ensure!(written == 3, "vector_search::recall: expected 3 chunks seeded, got {written}",);

    // top-k = 1 — we only need the nearest neighbour to confirm the
    // index is searchable. Stronger ordering assertions live in
    // as_of_parity (cross-backend ordering equality is the harder gate).
    let hits =
        storage.vector_search(run.scope(), "chunks", &target_vec, 1, None, None, false).await?;
    anyhow::ensure!(
        !hits.is_empty(),
        "vector_search::recall: backend returned 0 hits for the seeded query",
    );
    Ok(())
}

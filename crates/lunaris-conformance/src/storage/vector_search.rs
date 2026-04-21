//! Plan 05-02 D-17 — `vector_search` conformance.
//!
//! Seeds 3 chunk vectors via `fixtures::seed_three_chunks` (B-3 fix —
//! defined in Task 1, called unconditionally here), queries with the
//! exact target vector, and asserts at least one hit comes back. Both
//! Moon and Postgres backends accept `index="chunks"` (the canonical
//! chunk index name across both per
//! `lunaris-storage-postgres/src/vector.rs:38-48` whitelist).

#![forbid(unsafe_code)]

use std::sync::Arc;

use lunaris_core::storage::StoragePort;

/// Vector dimensionality for the conformance fixture. 16 components is
/// well below both backends' `max_vector_dim` (Moon=768, Postgres=1536)
/// while still being expressive enough to exercise distance ordering.
const FIXTURE_DIM: usize = 16;

pub async fn recall(storage: &Arc<dyn StoragePort>) -> anyhow::Result<()> {
    // Predictable target vector: monotonic ramp [0.0, 0.1, 0.2, ..., 1.5].
    // The fixture helper seeds index-0 with this exact vector and indices
    // 1+2 with perturbed copies, so a query for `target_vec` MUST rank
    // the index-0 chunk first when the backend honours cosine distance.
    let target_vec: Vec<f32> = (0..FIXTURE_DIM).map(|i| i as f32 * 0.1).collect();

    let written = crate::fixtures::seed_three_chunks(storage, &target_vec).await?;
    anyhow::ensure!(
        written == 3,
        "vector_search::recall: expected 3 chunks seeded, got {written}",
    );

    // top-k = 1 — we only need the nearest neighbour to confirm the
    // index is searchable. Stronger ordering assertions live in
    // as_of_parity (cross-backend ordering equality is the harder gate).
    let hits = storage
        .vector_search("chunks", &target_vec, 1, None, None, false)
        .await?;
    anyhow::ensure!(
        !hits.is_empty(),
        "vector_search::recall: backend returned 0 hits for the seeded query",
    );
    Ok(())
}

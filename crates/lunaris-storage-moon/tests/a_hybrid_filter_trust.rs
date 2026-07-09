//! moon-v051-perf-exploit W1-4 — HYBRID FILTER push-down trust test.
//!
//! Lunaris's production Moon-native RRF path
//! (`crates/lunaris-retrieve/src/fusion.rs::fuse_via_moon_native`) already
//! translates the Lunaris `Filter` tree into a Moon `HybridFilter` and passes
//! it to `client.text().hybrid_search(..., filter)` (CHANGE H/I/J, landed
//! before this workstream). This test does NOT re-test that translation (see
//! `fusion.rs`'s own unit tests for that) — it proves the SERVER-SIDE
//! capability those call sites depend on: Moon's PR #174 claim that a HYBRID
//! `FILTER` clause constrains BOTH the BM25 stream AND the dense-KNN stream,
//! not just BM25 (the historical "bypass bug" `fusion.rs` documents).
//!
//! `lunaris-retrieve` cannot be exercised here (this crate does not, and per
//! the dependency direction should not, depend on it) — instead this test
//! calls the exact same `moon-client` SDK method (`typed().text().
//! hybrid_search`) with the exact same index-naming convention
//! (`lunaris_{scope}_{kind}_idx`) and schema (`content` TEXT, `source` TAG,
//! `vec` VECTOR — all provisioned by this crate's own
//! `create_lunaris_index_named`), seeded through the PRODUCTION
//! `atomic_write` path, so a regression in Moon's server-side filter
//! push-down would be caught here just as it would in the real recall path.
//!
//! ## Design: proving the DENSE branch specifically
//!
//! `weights = [0.0, 1.0, 0.0]` (BM25 weight zero, dense weight one) so a
//! foreign-scope-tagged document that is DELIBERATELY the closest vector to
//! the query can ONLY leak through the dense stream — if the server applied
//! FILTER to BM25 alone (the historical bug), this doc would still win.
//! `text_query = "payload"` (shared by both seeded docs) keeps the BM25 stream's candidate pool
//! unconstrained (both docs' `content` shares the word "payload"), isolating
//! the filter as the only exclusion mechanism — Moon's HYBRID text analyzer
//! rejects a bare `"*"` ("empty query after analysis"), unlike the plain
//! FT.SEARCH filter-expression path elsewhere in this crate.
//!
//! A companion "unfiltered control" query proves the harness is
//! discriminating: without the FILTER clause, the foreign doc DOES win (it's
//! the closest vector) — so the filtered assertion is a real proof, not a
//! vacuously-true one.
//!
//! ```bash
//! MOON_URL=moon://localhost:7801 \
//!   cargo test -p lunaris-storage-moon --features moon-it --test a_hybrid_filter_trust -- --nocapture
//! ```

#![cfg(feature = "moon-it")]

use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::WriteOp;
use lunaris_storage_moon::MoonStorage;
use moon::text::HybridFilter;
use serde_json::json;

const DIM: usize = 768;

fn moon_url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:7801".to_string())
}

async fn connect_or_skip() -> Option<MoonStorage> {
    match MoonStorage::connect(&moon_url()).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("MOON_URL not reachable ({e}); SKIP");
            None
        }
    }
}

fn doc_id(tag: u8) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[0] = tag;
    id[15] = b'H';
    id
}

/// §1 DISCRIMINATOR — HYBRID FILTER constrains the DENSE branch. A foreign
/// -tagged doc that is the closest vector to the query MUST NOT appear in
/// filtered results, even at `weights = [0.0, 1.0, 0.0]` (pure dense).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hybrid_filter_excludes_foreign_source_from_dense_branch() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = Scope::new(format!("hybrid-trust-{}", ulid::Ulid::new())).expect("valid scope");

    // Query vector: unit-ish vector, first component dominant.
    let mut query: Vec<f32> = vec![0.0; DIM];
    query[0] = 1.0;

    // "Attacker" doc: EXACT copy of the query vector (closest possible
    // match) but tagged with a source the query will NOT filter on. If the
    // server-side FILTER only constrained BM25 (the historical bypass bug),
    // this doc would still rank #1 on the dense stream and leak through.
    let attacker_id = doc_id(0xAA);
    // "Legit" doc: same source as the filter target, but a WEAKER vector
    // match (orthogonal-ish) so it would lose the dense race unfiltered.
    let legit_id = doc_id(0x11);
    let mut legit_vec: Vec<f32> = vec![0.0; DIM];
    legit_vec[1] = 1.0;

    let ops = vec![
        WriteOp::VectorUpsert {
            index: "chunks".into(),
            id: attacker_id.clone(),
            embedding: query.clone(),
            metadata: json!({"text": "attacker payload", "source": "attacker-source"}),
        },
        WriteOp::VectorUpsert {
            index: "chunks".into(),
            id: legit_id.clone(),
            embedding: legit_vec,
            metadata: json!({"text": "legit payload", "source": "trusted-source"}),
        },
    ];
    moon.atomic_write(&scope, &ops).await.expect("seed write");

    let idx_name = format!("lunaris_{}_chunks_idx", scope.as_str());
    let typed = moon.client().typed();
    let mut text = typed.text();

    // Pure-dense weighting: BM25 contributes nothing to the fused score, so
    // if the attacker doc leaks through, it can ONLY be via the dense
    // stream being unfiltered.
    let weights: [f64; 3] = [0.0, 1.0, 0.0];

    // ── Control: UNFILTERED query must surface the attacker doc as the
    // top (or only) hit — proves the setup is discriminating. ──
    let unfiltered = text
        .hybrid_search(&idx_name, "payload", &query, "vec", None, 5, weights, None)
        .await
        .expect("unfiltered hybrid_search must succeed");
    assert!(
        unfiltered.iter().any(|h| h.key.ends_with(&hex::encode(&attacker_id))),
        "control check failed: the attacker doc (exact vector match) must appear \
         WITHOUT a filter, or this test cannot discriminate a broken filter from a passing one"
    );

    // ── Filtered query: `source == trusted-source` must exclude the
    // attacker doc from the DENSE branch, even though it's the closest
    // vector and BM25 carries zero weight. ──
    let filter =
        HybridFilter::Tag { field: "source".to_string(), value: "trusted-source".to_string() };
    let filtered = text
        .hybrid_search(&idx_name, "payload", &query, "vec", None, 5, weights, Some(&filter))
        .await
        .expect("filtered hybrid_search must succeed");

    assert!(
        filtered.iter().any(|h| h.key.ends_with(&hex::encode(&legit_id))),
        "the legit (trusted-source) doc must still be returned under the filter"
    );
    assert!(
        !filtered.iter().any(|h| h.key.ends_with(&hex::encode(&attacker_id))),
        "SERVER-SIDE LEAK: the attacker doc (source=attacker-source, zero BM25 weight, \
         closest possible dense match) appeared in FILTERED results — PR #174's \
         both-branches guarantee did not hold on this Moon build. Got hits: {:?}",
        filtered.iter().map(|h| &h.key).collect::<Vec<_>>()
    );
}

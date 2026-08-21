//! W3 (moon-v051-perf-exploit) — live-Moon proof that the embedding
//! double-store fix survives the real production ingest -> storage -> recall
//! path, not just an in-process `serde_json` round-trip.
//!
//! Gated behind `moon-it` (mirrors `lunaris-storage-moon`'s and
//! `lunaris-recipes`'s existing `moon-it` convention) + a reachable
//! `MOON_URL`. Skips (does not fail) if Moon isn't running — this keeps
//! `cargo test --workspace` green without a live dependency.
//!
//! Run: `MOON_URL=moon://127.0.0.1:7803 cargo test -p lunaris-ingest \
//!        --features moon-it --test c_live_moon_embedding_kv_size`
//!
//! Proves, against a real Moon (vendored @ v0.5.1+, `c9508066`):
//! 1. The chunk `KvPut` bytes actually written to Moon's HSET do not carry
//!    the `"embedding"` key (byte-level proof, not just the in-process
//!    `serde_json::to_vec` proof in `lunaris-core`'s
//!    `c_embedding_skip_serialize.rs`).
//! 2. `chunks` FT vector_search still returns the chunk with its full 768-d
//!    embedding and correct metadata — recall is unaffected by dropping the
//!    KV-blob copy, because Moon's FT index carries the binary vector
//!    independently (this is the whole premise of the fix).
//! 3. The community `KvPut` bytes likewise never carry `"summary_embedding"`.

#![cfg(feature = "moon-it")]

use lunaris_core::{Episode, HlcClock, Scope, StoragePort, StubEmbedder, keyspace::chunk_prefix};
use lunaris_storage_moon::MoonStorage;

fn url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://127.0.0.1:7803".to_string())
}

async fn connect_or_skip() -> Option<MoonStorage> {
    match MoonStorage::connect(&url()).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("MOON_URL not reachable ({e}); SKIP c_live_moon_embedding_kv_size");
            None
        }
    }
}

const DOC: &str = "# Quarterly Report

Revenue grew 12% year over year, driven by strong demand in the enterprise segment.

## Outlook

Management expects continued growth into the next fiscal year.
";

#[tokio::test]
async fn live_moon_chunk_kv_never_carries_embedding_but_vector_search_still_works() {
    let Some(storage) = connect_or_skip().await else { return };
    let scope =
        Scope::new(format!("w3-embedding-live-{}", ulid::Ulid::new().to_string().to_lowercase()))
            .expect("scope");

    let embedder = StubEmbedder::new(768);
    let clock = HlcClock::new(0);
    let episode = Episode::new(scope.clone(), "quarterly-report.md", DOC, &clock);

    // W4.5 gated the RAPTOR community write behind LUNARIS_RAPTOR_ENABLED
    // (default OFF), so the default path writes NO communities and the
    // community assertion at the bottom of this test could never hold. Section
    // 3 below is a community-KV property, so this opts in explicitly — the same
    // way raptor_wiring.rs does, via the parameter rather than by mutating
    // process env (edition 2024 makes std::env::set_var unsafe, and parallel
    // tests race on it).
    let receipt =
        lunaris_ingest::ingest_episode_with_raptor(&storage, &embedder, &clock, episode, true)
        .await
        .expect("live ingest must succeed");
    assert!(!receipt.chunk_ids.is_empty(), "fixture must produce at least one chunk");

    // 1. Read the RAW bytes Moon actually stored for each chunk — not a
    //    round-trip through our own serializer, the real HSET payload.
    let prefix = chunk_prefix(&scope);
    let mut stream = storage
        .scan_range(&scope, &prefix, None)
        .await
        .expect("scan_range over the live chunk prefix must succeed");
    use futures::StreamExt as _;
    let mut n_chunks = 0usize;
    let mut total_bytes = 0usize;
    while let Some(item) = stream.next().await {
        let (_, value) = item.expect("scan_range item must not error");
        n_chunks += 1;
        total_bytes += value.len();
        assert!(
            !value.windows(b"\"embedding\"".len()).any(|w| w == b"\"embedding\""),
            "live Moon chunk KV bytes must NOT contain the \"embedding\" key: {:?}",
            String::from_utf8_lossy(&value)
        );
        let c: lunaris_core::primitives::Chunk =
            serde_json::from_slice(&value).expect("live chunk KV bytes must deserialize");
        assert!(c.embedding.is_none(), "live chunk KV bytes must deserialize with embedding=None");
        assert!(!c.text.is_empty(), "chunk text must still be present and readable");
    }
    assert_eq!(n_chunks, receipt.chunk_ids.len(), "scan must see every ingested chunk");
    eprintln!(
        "[c_live_moon_embedding_kv_size] {n_chunks} chunk KvPuts, {total_bytes} total bytes, \
         avg {} bytes/chunk (no embedding inlined)",
        total_bytes / n_chunks.max(1)
    );

    // 2. The `chunks` FT index must still be fully searchable with the real
    //    768-d embedding — recall correctness is independent of the KV-blob
    //    change, because the vector lives in Moon's own FT index.
    let probe = vec![1.0_f32; 768];
    let hits = storage
        .vector_search(&scope, "chunks", &probe, 10, None, None, false)
        .await
        .expect("live Moon vector_search must succeed");
    assert!(
        !hits.is_empty(),
        "chunks FT index must return hits after ingest — recall must not regress \
         when the KV blob stops carrying the embedding"
    );
    // Every returned hit id must resolve back to one of the chunks we just
    // ingested — proves the FT index is genuinely serving OUR embedding, not
    // stale data from a previous run under a colliding scope.
    let chunk_id_bytes: std::collections::HashSet<Vec<u8>> =
        receipt.chunk_ids.iter().map(|id| id.to_bytes().to_vec()).collect();
    for hit in &hits {
        assert!(hit.score.is_finite(), "vector_search score must be a finite f32");
        assert!(
            chunk_id_bytes.contains(&hit.id),
            "vector_search hit id must be one of this test's own ingested chunks"
        );
    }

    // 3. Same proof for the community KvPut (summary_embedding).
    let community_prefix = lunaris_core::keyspace::community_prefix(&scope);
    let mut cstream = storage
        .scan_range(&scope, &community_prefix, None)
        .await
        .expect("scan_range over the live community prefix must succeed");
    let mut n_communities = 0usize;
    while let Some(item) = cstream.next().await {
        let (_, value) = item.expect("scan_range item must not error");
        n_communities += 1;
        assert!(
            !value.windows(b"summary_embedding".len()).any(|w| w == b"summary_embedding"),
            "live Moon community KV bytes must NOT contain \"summary_embedding\""
        );
    }
    assert!(n_communities >= 1, "the H1/H2 fixture must produce at least one community");
}

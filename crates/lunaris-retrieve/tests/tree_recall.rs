//! N5/B2 — RAPTOR tree retrieval integration test.
//!
//! ## What this proves
//!
//! This test demonstrates the discriminating property of the `Tree` operator:
//! a whole-document query where flat chunk retrieval at small `k` misses chunks
//! that tree retrieval (community summary → leaf descent) finds.
//!
//! ## Test strategy (with `StubEmbedder`)
//!
//! `StubEmbedder` produces deterministic hash-based vectors per input string,
//! not zero vectors — so the query text's embedding genuinely differs from
//! each individual chunk's embedding. The community summary text aggregates
//! the child chunks, producing its own distinct hash-based vector.
//!
//! **Discrimination setup:**
//! - Ingest a multi-section markdown document → RAPTOR builds ≥2 leaf
//!   communities, each with several member chunk IDs.
//! - Flat chunk retrieval at `k=1` returns exactly 1 chunk — whichever single
//!   chunk most resembles the query vector.
//! - Tree retrieval at community `k=1` finds the top community and descends
//!   to return ALL member chunks of that community.
//! - Because the document has >1 chunk per community, tree retrieval returns
//!   more chunks than flat retrieval.
//!
//! ## Requirements
//!
//! Live Moon at `MOON_URL` (default `moon://127.0.0.1:6380`).
//! Run with:
//! ```bash
//! MOON_URL=moon://127.0.0.1:6380 \
//!   cargo test -p lunaris-retrieve --test tree_recall -- --nocapture
//! ```
//! The test skips gracefully when Moon is not reachable.

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::storage::keyword::KeywordHit;
use lunaris_core::{
    Episode, Filter, Hlc, HlcClock, KeywordPort, Scope, StorageError, StoragePort, StubEmbedder,
};
use lunaris_ingest::pipeline::ingest_episode_with_receipt;
use lunaris_retrieve::operators::tree::Tree;
use lunaris_retrieve::{Query, QueryContext, Retriever, SourceOp, Vector};
use lunaris_storage_moon::MoonStorage;
use ulid::Ulid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn moon_url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://127.0.0.1:6380".into())
}

/// Return `true` if Moon is reachable at `url`.
fn probe_moon(url: &str) -> bool {
    let host_port = url
        .strip_prefix("moon://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("127.0.0.1:6380");
    // Try numeric parse first; fall back to hostname resolution.
    if let Ok(addr) = host_port.parse::<std::net::SocketAddr>() {
        TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
    } else {
        use std::net::ToSocketAddrs;
        host_port
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next())
            .map(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok())
            .unwrap_or(false)
    }
}

/// A multi-section markdown document large enough to guarantee ≥2 chunks.
///
/// The surrogate token counter used by `ingest_episode_with_receipt` estimates
/// tokens as `words × 1.3`. `DEFAULT_TARGET_TOKENS = 500`, so each section
/// needs ~385 words to produce one chunk. This document has ~200 words per
/// section — two sections → ~400 surrogate-token words total → likely 1 chunk.
///
/// To guarantee ≥2 chunks we need >500 surrogate tokens total. We pad each
/// section to ~300 words (→ ~390 tokens), giving ~780 tokens across two
/// sections → 2 chunks guaranteed.
///
/// RAPTOR tree topology for this doc:
/// ```
/// H1 "Agent Memory Architecture"      ← root community; members = [H2_A_comm, H2_B_comm]
///   H2 "Section A: Core Design"       ← leaf community A; members = [chunk_A1, chunk_A2, ...]
///   H2 "Section B: Performance"       ← leaf community B; members = [chunk_B1, chunk_B2, ...]
/// ```
///
/// Tree retrieval at community `k=1, depth=2`:
/// - Depth 0 BFS: vector-search finds root community (whole-doc query matches H1 summary)
/// - Depth 1 BFS: root.members = [H2_A_comm, H2_B_comm] → both are sub-communities, enqueue
/// - Depth 2 BFS: H2_A.members = [chunk_ids], H2_B.members = [chunk_ids] → emit all leaf chunks
///
/// Flat retrieval at chunk `k=1`: returns exactly 1 chunk (the single best-matching chunk).
/// Tree retrieval returns ALL leaf chunks across both sections → tree > flat.
const MULTI_SECTION_DOC: &str = r#"# Agent Memory Architecture

## Section A: Core Design

The agent memory system uses a bi-temporal MVCC store. Each observation is
recorded with both a valid-time and a transaction-time. This dual timestamp
enables point-in-time queries and auditing of historical states. The bi-temporal
model means we can query what was known at any point in transaction time, and
what was true at any point in valid time. This is essential for agent systems
that need to reconcile information gathered at different times and understand
how the world state has changed over time relative to when observations were made.

The retrieval pipeline fuses semantic vector search with BM25 keyword search
using Reciprocal Rank Fusion. Communities of related chunks are organized
hierarchically by the RAPTOR algorithm, which builds summary nodes that
aggregate the semantic content of their child chunks into a single vector. The
RRF fusion formula combines per-branch reciprocal ranks into a unified score that
is robust to score scale differences between vector cosine similarity and BM25
normalized scores. Each branch contributes independently, and the fused score
reflects consensus across retrieval strategies.

Memory isolation between agents uses scope partitioning. Each agent receives
a unique scope key that prefixes all its KV store entries and FT index slots,
preventing cross-agent data leakage at the storage layer with no extra joins.
The scope key is encoded directly into the Redis keyspace and Moon FT index
slot names so that partitioning is enforced at the data layer, not the
application layer. This design means that a misconfigured application cannot
accidentally read another agent's memories — the backend enforces isolation.

The MVCC model supports concurrent writes from multiple agent instances without
locking. Each write produces a new version stamped with a hybrid logical clock
timestamp. Readers specify an as-of timestamp and receive a consistent snapshot
of all versions whose valid-time interval includes that timestamp and whose
transaction-time is before the requested transaction-time. This enables
point-in-time historical queries without expensive snapshot isolation overhead.

## Section B: Performance Characteristics

The system targets sub-25ms recall latency over millions of bi-temporal facts.
Moon provides native HNSW vector search with cosine similarity, accelerating
the vector branch of the hybrid retrieval pipeline to single-digit milliseconds.
The HNSW index supports approximate nearest-neighbor search with tunable
recall-latency tradeoffs. At the default M=16 and ef=200 construction
parameters, Moon achieves better than 99% recall at sub-5ms query latency
for indexes with up to 10 million vectors.

Community summary embeddings allow whole-document queries to bypass flat chunk
retrieval entirely. A query about the overall themes of a document will match
the community summary node rather than individual chunks, which may not score
high enough individually to appear in the top-k flat results without tree
retrieval. This is the core RAPTOR insight: summarization at multiple granularity
levels allows retrieval to match at the right abstraction level for each query.

The embedding dimension is 768 for the granite-embedding-311m-multilingual-r2
model. Community summaries are embedded using the same model, ensuring cosine
similarity scores are comparable across both the chunks and communities indices.
The ModernBERT architecture used by the granite model provides excellent
multilingual coverage and strong performance on asymmetric retrieval tasks where
the query is short and the documents are long. The model runs on CPU without
requiring GPU acceleration, making it suitable for edge deployment scenarios.

The memory system also provides a graph layer for relational queries. Entity
extraction identifies named entities and their relationships from each episode.
Graph traversal starting from matched entities provides additional context that
pure vector retrieval would miss. The graph layer is opt-in: callers that do
not need relational context use the simpler vector or hybrid retrieval paths
and pay no overhead for the graph infrastructure.
"#;

/// Stub keyword port — tree retrieval does not use keyword search.
struct NoKeyword;

#[async_trait]
impl KeywordPort for NoKeyword {
    async fn keyword_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(vec![])
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Prove that `Tree` returns more chunks than flat `Vector` on a whole-document
/// query, demonstrating RAPTOR's hierarchical retrieval advantage.
///
/// Discrimination criterion:
/// - `flat_hits.len() == 1`  (k=1 flat chunk retrieval)
/// - `tree_hits.len() >= 2`  (community k=1 → descends to multiple leaf chunks)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tree_retrieval_beats_flat_on_whole_document_query() {
    let url = moon_url();
    if !probe_moon(&url) {
        eprintln!("SKIP tree_recall: Moon not reachable at {url}");
        return;
    }

    // ------------------------------------------------------------------ setup
    // Unique scope so parallel test runs (or re-runs) don't interfere.
    let scope_str = format!("b2tree{}", &Ulid::new().to_string()[..8]);
    let scope = Scope::new(&scope_str).expect("scope must be valid");

    // StubEmbedder: deterministic hash-based 768-d vectors. No candle required.
    let dim = 768_usize;
    let embedder = Arc::new(StubEmbedder::new(dim));

    // Connect to Moon with the same dim the embedder reports.
    let storage = Arc::new(
        MoonStorage::connect_with_dim(&url, dim)
            .await
            .expect("MoonStorage::connect_with_dim must succeed for live Moon"),
    );
    let keyword: Arc<dyn KeywordPort> = Arc::new(NoKeyword);
    let clock = HlcClock::new(0);

    // ------------------------------------------------------------------ ingest
    // Ingest the multi-section document under our unique scope. RAPTOR builds
    // the community hierarchy during ingest; Phase-30 B1 embeds each community
    // summary so the `communities` FT index is populated.
    let episode = Episode::new(scope.clone(), "test:multi-section-doc", MULTI_SECTION_DOC, &clock);

    let receipt = ingest_episode_with_receipt(
        storage.as_ref() as &dyn StoragePort,
        embedder.as_ref(),
        &clock,
        episode,
    )
    .await
    .expect("ingest must succeed");

    let chunk_ids = receipt.chunk_ids;
    assert!(
        chunk_ids.len() >= 2,
        "doc must produce at least 2 chunks for the test to discriminate; got {}",
        chunk_ids.len()
    );

    eprintln!("Ingested {} chunks, episode_id={}", chunk_ids.len(), receipt.episode_id);

    // ------------------------------------------------------------------ flat retrieval (k=1)
    // A whole-document query about the overall themes of the document.
    // At k=1, flat chunk retrieval returns exactly ONE chunk — whichever single
    // chunk most resembles the query vector.
    let query_text = "What are the main themes and architectural decisions in this document?";

    let flat_query = Query { text: query_text.into(), k: 1, filter: None, as_of: None };
    let flat_ctx = QueryContext::new(
        flat_query,
        scope.clone(),
        embedder.clone(),
        storage.clone() as Arc<dyn StoragePort>,
        keyword.clone(),
    );
    let flat_op = Vector::new("chunks", 1);
    let flat_raw = flat_op.retrieve(&flat_ctx).await.expect("flat vector_search must succeed");

    eprintln!("Flat chunk retrieval (k=1): {} hits", flat_raw.len());
    assert_eq!(
        flat_raw.len(),
        1,
        "flat retrieval at k=1 must return exactly 1 chunk; got {}",
        flat_raw.len()
    );
    assert!(
        flat_raw.iter().all(|h| h.source_op == SourceOp::Vector),
        "flat hits must all be tagged SourceOp::Vector"
    );

    // ------------------------------------------------------------------ tree retrieval (community k=1)
    // Tree retrieval at community k=1: find the 1 nearest community summary,
    // then descend to ALL its leaf chunks. Because the community aggregates
    // multiple chunks, we expect more than 1 hit.
    let tree_query = Query { text: query_text.into(), k: 1, filter: None, as_of: None };
    let tree_ctx = QueryContext::new(
        tree_query,
        scope.clone(),
        embedder.clone(),
        storage.clone() as Arc<dyn StoragePort>,
        keyword.clone(),
    );
    // depth=2: root community (matched by whole-doc query) has sub-communities as
    // members (the H2 sections); depth=2 descends through them to leaf chunks.
    let tree_op = Tree::new("communities", 1).with_depth(2);
    let tree_raw = tree_op.retrieve(&tree_ctx).await.expect("tree retrieval must succeed");

    eprintln!("Tree retrieval (community k=1, depth=2): {} hits", tree_raw.len());
    eprintln!(
        "  flat chunk ids: {:?}",
        flat_raw.iter().map(|h| hex::encode(&h.id)).collect::<Vec<_>>()
    );
    eprintln!(
        "  tree chunk ids: {:?}",
        tree_raw.iter().map(|h| hex::encode(&h.id)).collect::<Vec<_>>()
    );

    // PRIMARY ASSERTION — discrimination criterion:
    // Tree returns MORE chunks than flat at the same budget (community k=1 vs chunk k=1).
    assert!(
        tree_raw.len() > flat_raw.len(),
        "tree retrieval (community k=1 → leaf descent) must return more chunks than flat \
         retrieval (chunk k=1); tree={}, flat={}. \
         This would mean the community has only 1 member chunk, which contradicts the \
         multi-section doc fixture. Verify RAPTOR built communities with >1 member chunk.",
        tree_raw.len(),
        flat_raw.len()
    );

    // SECONDARY ASSERTIONS — correctness properties:

    // All tree hits must be tagged SourceOp::Tree.
    assert!(
        tree_raw.iter().all(|h| h.source_op == SourceOp::Tree),
        "all tree hits must be tagged SourceOp::Tree; got: {:?}",
        tree_raw.iter().map(|h| h.source_op).collect::<Vec<_>>()
    );

    // Tree hits must be valid chunk IDs from the ingested episode.
    let ingested_ids: std::collections::HashSet<Vec<u8>> =
        chunk_ids.iter().map(|id: &Ulid| id.to_bytes().to_vec()).collect();
    for hit in &tree_raw {
        assert!(
            ingested_ids.contains(&hit.id),
            "tree hit id must be a chunk from the ingested episode; id={}",
            hex::encode(&hit.id)
        );
    }

    // The chunk returned by flat retrieval must also appear in tree results
    // (it's a member of the nearest community, so the community contains it).
    let flat_id = &flat_raw[0].id;
    let tree_ids: std::collections::HashSet<&Vec<u8>> = tree_raw.iter().map(|h| &h.id).collect();
    assert!(
        tree_ids.contains(flat_id),
        "the chunk returned by flat retrieval must also appear in tree retrieval results \
         (it's a member of the nearest community). flat_id={}, tree_ids={:?}",
        hex::encode(flat_id),
        tree_raw.iter().map(|h| hex::encode(&h.id)).collect::<Vec<_>>()
    );

    eprintln!(
        "PASS: tree={} chunks > flat={} chunks; all SourceOp::Tree; flat chunk in tree results",
        tree_raw.len(),
        flat_raw.len()
    );
}

/// Smoke test: `Tree` against an empty communities index (unique scope with no
/// data) returns an empty result rather than panicking.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn tree_retrieval_empty_index_returns_empty() {
    let url = moon_url();
    if !probe_moon(&url) {
        eprintln!("SKIP tree_empty_smoke: Moon not reachable at {url}");
        return;
    }

    // Unique scope with no data ingested under it → empty communities index.
    let scope_str = format!("b2empty{}", &Ulid::new().to_string()[..8]);
    let scope = Scope::new(&scope_str).expect("valid scope");
    let dim = 768_usize;
    let embedder = Arc::new(StubEmbedder::new(dim));

    let storage = Arc::new(
        MoonStorage::connect_with_dim(&url, dim).await.expect("MoonStorage::connect_with_dim"),
    );
    let keyword: Arc<dyn KeywordPort> = Arc::new(NoKeyword);

    let query = Query::text("anything");
    let ctx = QueryContext::new(query, scope, embedder, storage as Arc<dyn StoragePort>, keyword);
    let tree_op = Tree::new("communities", 5);
    let result = tree_op.retrieve(&ctx).await.expect("empty tree retrieval must not error");
    assert!(
        result.is_empty(),
        "empty communities index must return empty hits; got {}",
        result.len()
    );
    eprintln!("PASS: empty communities index returns 0 hits");
}

/// Pure-unit: `SourceOp::Tree` is distinct from all other variants.
/// Does not require a live backend.
#[test]
fn source_op_tree_is_distinct_from_all_other_variants() {
    assert_ne!(SourceOp::Tree, SourceOp::Vector);
    assert_ne!(SourceOp::Tree, SourceOp::Keyword);
    assert_ne!(SourceOp::Tree, SourceOp::Fused);
    assert_ne!(SourceOp::Tree, SourceOp::Reranked);
    assert_ne!(SourceOp::Tree, SourceOp::Graph);
}

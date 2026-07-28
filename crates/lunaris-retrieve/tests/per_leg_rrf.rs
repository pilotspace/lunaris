//! Per-leg RRF bucketing + Navigate leg hygiene (expert review 2026-07-28,
//! findings F1/F3/F4 — the 76.0%-vs-82.4% graph-ON regression mechanism).
//!
//! `client_side_rrf_weighted` buckets by `SourceOp` ONLY, so `hybrid_root`'s
//! four legs collapse into two buckets:
//! - Navigate hits (tagged `SourceOp::Vector`) share the chunk-vector bucket,
//!   and Moon scores graph-expanded nodes `hops × hop_penalty` (a constant,
//!   relevance-free 0.1/0.2 → 0.909/0.833 after 1/(1+d)) — beating every
//!   genuine chunk cosine (~0.4-0.8), so gold chunks get demoted by junk.
//! - Both BM25 legs are min-max normalized PER CALL (each leg's top hit is
//!   exactly 1.0) and share the Keyword bucket — a junk facts leg interleaves
//!   ~1:1 with the chunks leg, halving gold chunks' keyword-RRF contribution.
//! - Navigate's hop-0 seeds are ENTITY ids with no KV row: hydrate_mixed
//!   drops them after `.top(k)`, so they consume fused/rerank/final slots and
//!   then vanish from the reader context.
//!
//! Contract under test (RED until the per-leg fix lands):
//! 1. legs bucket by (SourceOp, `metadata["index"]` tag) — rank-based RRF
//!    per leg makes cross-leg score magnitudes irrelevant;
//! 2. Navigate drops hop-0 seeds before fusion;
//! 3. a gold chunk ranked top of two chunk legs out-fuses single-leg junk
//!    (the exact adversarial shape that produced the regression).

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, NavigateHit, NavigateSpec, QueueMsg, Row, VectorHit,
    WriteOp,
};
use lunaris_core::{
    Hlc, NoopEmbedder, Scope, StorageCapabilities, StorageError, StoragePort,
};
use lunaris_retrieve::operators::{QueryContext, Retriever};
use lunaris_retrieve::{Query, hybrid_root};

// ─── ids ─────────────────────────────────────────────────────────────────────

const GOLD: &[u8; 16] = b"GOLD_CHUNK_00001";
const D1: &[u8; 16] = b"DISTRACT_CHUNK_1";
const D2: &[u8; 16] = b"DISTRACT_CHUNK_2";
const D3: &[u8; 16] = b"DISTRACT_CHUNK_3";
const JF1: &[u8; 16] = b"JUNK_FACT_000001";
const JF2: &[u8; 16] = b"JUNK_FACT_000002";
const JF3: &[u8; 16] = b"JUNK_FACT_000003";
const ENT_SEED: &[u8; 16] = b"ENTITY_SEED_0001";
const HOP_FACT1: &[u8; 16] = b"HOP_FACT_0000001";
const HOP_FACT2: &[u8; 16] = b"HOP_FACT_0000002";

fn vh(id: &[u8; 16], score: f32) -> VectorHit {
    VectorHit {
        id: id.to_vec(),
        score,
        rerank_applied: false,
        metadata: serde_json::json!({}),
    }
}

fn kh(id: &[u8; 16], score: f32) -> KeywordHit {
    KeywordHit { id: id.to_vec(), score, raw_score: score * 7.3, metadata: serde_json::json!({}) }
}

fn nh(id: &[u8; 16], hop_depth: u32, final_score: f32) -> NavigateHit {
    NavigateHit { id: id.to_vec(), vec_score: final_score, hop_depth, final_score }
}

// ─── mock storage ────────────────────────────────────────────────────────────

struct LegStorage;

#[async_trait]
impl StoragePort for LegStorage {
    async fn read_as_of(
        &self,
        _: &Scope,
        _: &[u8],
        _: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(None)
    }
    async fn atomic_write(&self, _: &Scope, _: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
    async fn vector_search(
        &self,
        _: &Scope,
        index: &str,
        _: &[f32],
        _: usize,
        _: Option<&Filter>,
        _: Option<Hlc>,
        _: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        // Post-vec_score-fix similarity semantics: genuine cosines are
        // small-to-mid magnitude, higher = better.
        Ok(match index {
            "chunks" => vec![vh(GOLD, 0.62), vh(D1, 0.55), vh(D2, 0.50)],
            _ => vec![],
        })
    }
    async fn vector_navigate(
        &self,
        _: &Scope,
        _: &str,
        _: &[f32],
        _: usize,
        _: &NavigateSpec,
    ) -> Result<Vec<NavigateHit>, StorageError> {
        // Moon's real shape: seeds carry genuine entity distances; expanded
        // nodes carry the relevance-free hops×0.1 constant (graph_expand.rs
        // hardcodes vec_score 0.0 → final_score = hop penalty only).
        Ok(vec![nh(ENT_SEED, 0, 0.45), nh(HOP_FACT1, 1, 0.10), nh(HOP_FACT2, 2, 0.20)])
    }
    async fn graph_traverse(
        &self,
        _: &Scope,
        _: &CypherQuery,
        _: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("LegStorage"))
    }
    async fn scan_range(
        &self,
        _: &Scope,
        _: &[u8],
        _: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(Box::pin(stream::empty()))
    }
    async fn publish(&self, _: &Scope, _: &str, _: u16, _: Bytes) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("LegStorage"))
    }
    async fn subscribe(
        &self,
        _: &Scope,
        _: &str,
        _: &str,
        _: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("LegStorage"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: true,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 768,
            native_rrf: false, // force the client-side RRF path under test
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: true, // Navigate takes the native path
        }
    }
}

#[async_trait]
impl KeywordPort for LegStorage {
    async fn keyword_search(
        &self,
        _: &Scope,
        index: &str,
        _: &str,
        _: usize,
        _: Option<&Filter>,
        _: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        // Per-call min-max normalization: each leg's top hit is ~1.0
        // regardless of raw relevance — the F3 shape. The gold chunk has a
        // BM25 vocabulary gap on the chunks leg (rank 2, not 1).
        Ok(match index {
            "chunks" => vec![kh(D2, 1.0), kh(GOLD, 0.95), kh(D3, 0.85)],
            "facts" => vec![kh(JF1, 0.99), kh(JF2, 0.97), kh(JF3, 0.96)],
            _ => vec![],
        })
    }
}

fn ctx() -> QueryContext {
    let s = Arc::new(LegStorage);
    QueryContext::new(
        Query::text("what did the user decide?"),
        Scope::new("per-leg-rrf").unwrap(),
        Arc::new(NoopEmbedder::new(8)),
        s.clone(),
        s,
    )
}

// ─── tests ───────────────────────────────────────────────────────────────────

/// The adversarial regression shape: gold chunk tops both chunk legs but is
/// junk-demoted inside the two shared buckets. Per-leg bucketing must rank
/// it first; single-bucket fusion ranks distractor D2 (top of the merged
/// keyword bucket + present in vector) or hop-constant facts above it.
#[tokio::test]
async fn gold_chunk_tops_fused_output_despite_cross_leg_junk() {
    let root = hybrid_root(6);
    let hits = root.retrieve(&ctx()).await.expect("hybrid_root retrieve");
    assert!(!hits.is_empty(), "fused output must not be empty");
    assert_eq!(
        hits[0].id,
        GOLD.to_vec(),
        "gold chunk must out-fuse single-leg junk; got top id {:?} (scores: {:?})",
        String::from_utf8_lossy(&hits[0].id),
        hits.iter()
            .map(|h| (String::from_utf8_lossy(&h.id).into_owned(), h.score))
            .collect::<Vec<_>>()
    );
}

/// Navigate's hop-0 seeds are entity ids with no KV row — hydration drops
/// them AFTER `.top(k)`, so every seed that reaches fusion steals a final
/// reader-context slot. The operator must drop them before fusion.
#[tokio::test]
async fn navigate_hop0_entity_seeds_never_reach_fusion() {
    let root = hybrid_root(6);
    let hits = root.retrieve(&ctx()).await.expect("hybrid_root retrieve");
    assert!(
        hits.iter().all(|h| h.id != ENT_SEED.to_vec()),
        "hop-0 entity seed must be dropped before fusion"
    );
    // The hop-expanded FACT nodes are the leg's payload — they must survive.
    assert!(
        hits.iter().any(|h| h.id == HOP_FACT1.to_vec()),
        "hop-expanded fact nodes must still surface"
    );
}

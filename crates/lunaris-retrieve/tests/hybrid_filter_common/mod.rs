//! Shared harness for the `moon-hybrid-filter-bypass` red suite.
//!
//! These integration tests prove that a `.filter()`'d hybrid recall on the
//! Moon-native fuse path NEVER returns a hit violating the filter. Today the
//! filter is rendered into the BM25 text-query (`compose_query_with_filter`,
//! `fusion.rs:168`) which (a) is a silent NO-OP on the BM25 branch and (b) is
//! ignored entirely by the dense-KNN branch — so foreign-source hits survive
//! RRF fusion. Each test below is RED today (the foreign source leaks) and is
//! designed to go GREEN once the cross-repo fix lands (Moon `HybridFilter` +
//! `lunaris-retrieve` CHANGE H/I — see TASK.md §3).
//!
//! ## Why a live Moon (not a mock)
//!
//! The bug lives at the Moon FT.SEARCH HYBRID boundary — a mock cannot
//! reproduce it. We connect to a live Moon at `MOON_URL` (default
//! `moon://127.0.0.1:6380`) and SKIP gracefully when no listener is reachable,
//! mirroring `tree_recall.rs`. The red is observed against a real Moon (CI /
//! local dev with a Moon up); the skip keeps `cargo test` green on hosts
//! without one.
//!
//! ## Satisfiability (walked the future fix through this harness)
//!
//! - `source` is written as a TAG field per chunk (`atomic.rs:204`), so
//!   Eq/StartsWith/Or on `source` become correct once Moon applies the filter
//!   to BOTH branches. RED now → GREEN after.
//! - `valid_time` is a declared NUMERIC field but the production ingest path
//!   never populates `valid_time_ms` (pipeline.rs:328-334). The And/ValidTime
//!   tests therefore DIRECT-WRITE chunks carrying `valid_time_ms` so the
//!   numeric leaf is real. This exercises the Moon/SDK numeric-filter
//!   capability (the task's scope) — NOT the ingest→valid_time wiring, which
//!   is a separate gap recorded in TASK.md §4/§7.
#![allow(dead_code)]

use std::collections::HashSet;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::storage::keyword::KeywordHit;
use lunaris_core::storage::types::WriteOp;
use lunaris_core::{
    Embedder, Episode, Filter, Hlc, HlcClock, KeywordPort, Scope, StorageError, StoragePort,
    StubEmbedder,
};
use lunaris_ingest::pipeline::ingest_episode_with_receipt;
use lunaris_retrieve::{Keyword, Query, QueryContext, RawHit, Retriever, Vector};
use lunaris_storage_moon::MoonStorage;
use ulid::Ulid;

/// Embedding dimension — matches `StubEmbedder` + the chunks FT index.
pub const DIM: usize = 768;

/// `valid_time_ms` sentinel inside the 2025 calendar window.
pub const VT_2025_MS: u64 = 1_700_000_000_000; // 2023-11 in real epoch ms; "2025 bucket" for the test
/// `valid_time_ms` sentinel inside the 2026 calendar window (strictly later).
pub const VT_2026_MS: u64 = 1_760_000_000_000;

// ---------------------------------------------------------------------------
// Moon reachability (skip-if-down, identical contract to tree_recall.rs)
// ---------------------------------------------------------------------------

pub fn moon_url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://127.0.0.1:6380".into())
}

pub fn probe_moon(url: &str) -> bool {
    let host_port = url
        .strip_prefix("moon://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("127.0.0.1:6380");
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

/// Connect to Moon, returning `None` (→ test SKIP) when no listener is up.
pub async fn connect() -> Option<Arc<MoonStorage>> {
    let url = moon_url();
    if !probe_moon(&url) {
        eprintln!("SKIP hybrid_filter: Moon not reachable at {url}");
        return None;
    }
    Some(Arc::new(
        MoonStorage::connect_with_dim(&url, DIM)
            .await
            .expect("MoonStorage::connect_with_dim must succeed for live Moon"),
    ))
}

/// Deterministic 768-d embedder — no candle required.
pub fn embedder() -> Arc<StubEmbedder> {
    Arc::new(StubEmbedder::new(DIM))
}

/// Unique scope so parallel runs / re-runs never collide.
pub fn unique_scope(prefix: &str) -> Scope {
    let s = format!("{prefix}{}", &Ulid::new().to_string()[..10]);
    Scope::new(&s).expect("scope must be valid")
}

// ---------------------------------------------------------------------------
// Keyword stub — the native fuse path never calls it, but QueryContext needs one.
// ---------------------------------------------------------------------------

pub struct NoKeyword;

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
// Ingest path (production-wired): writes `source` TAG per chunk.
// ---------------------------------------------------------------------------

/// Ingest one episode under `source`; returns the chunk ULIDs it produced.
pub async fn ingest_source(
    storage: &Arc<MoonStorage>,
    embedder: &Arc<StubEmbedder>,
    clock: &Arc<HlcClock>,
    scope: &Scope,
    source: &str,
    text: &str,
) -> Vec<Ulid> {
    let episode = Episode::new(scope.clone(), source, text, clock);
    let receipt = ingest_episode_with_receipt(
        storage.as_ref() as &dyn StoragePort,
        embedder.as_ref(),
        clock,
        episode,
    )
    .await
    .expect("ingest must succeed");
    receipt.chunk_ids
}

// ---------------------------------------------------------------------------
// Direct-write path (test-only): full control over source + valid_time.
// ---------------------------------------------------------------------------

/// One synthetic chunk to write straight into the chunks FT index.
pub struct ChunkSpec {
    pub source: String,
    pub valid_time_ms: Option<u64>,
    pub text: String,
}

impl ChunkSpec {
    pub fn new(source: &str, valid_time_ms: Option<u64>, text: &str) -> Self {
        Self { source: source.into(), valid_time_ms, text: text.into() }
    }
}

/// Write `specs` as chunks via a single `atomic_write` (which `ensure_scope`s
/// the per-scope chunks index, declaring `source` TAG + `valid_time` NUMERIC).
/// Returns the chunk ULIDs in input order.
pub async fn direct_write_chunks(
    storage: &Arc<MoonStorage>,
    embedder: &Arc<StubEmbedder>,
    scope: &Scope,
    specs: &[ChunkSpec],
) -> Vec<Ulid> {
    let texts: Vec<&str> = specs.iter().map(|s| s.text.as_str()).collect();
    let embeddings = embedder.embed_batch(&texts).await.expect("embed_batch must succeed");

    let mut ids = Vec::with_capacity(specs.len());
    let mut ops = Vec::with_capacity(specs.len());
    for (spec, embedding) in specs.iter().zip(embeddings) {
        let id = Ulid::new();
        ids.push(id);
        let mut metadata = serde_json::json!({
            "source": spec.source,
            "text": spec.text,
        });
        if let Some(ms) = spec.valid_time_ms {
            metadata["valid_time_ms"] = serde_json::json!(ms);
        }
        ops.push(WriteOp::VectorUpsert {
            index: "chunks".to_string(),
            id: id.to_bytes().to_vec(),
            embedding,
            metadata,
        });
    }
    storage.atomic_write(scope, &ops).await.expect("atomic_write must succeed");
    ids
}

// ---------------------------------------------------------------------------
// Recall through the Moon-native hybrid fuse path.
// ---------------------------------------------------------------------------

/// Issue `Vector("chunks") AND Keyword("chunks") |> fuse_rrf` with `filter`,
/// routed through `fuse_via_moon_native` (gate: native_rrf + moon_storage +
/// Vector+Keyword same index). Returns the fused hits.
pub async fn recall_hybrid(
    storage: &Arc<MoonStorage>,
    embedder: &Arc<StubEmbedder>,
    scope: &Scope,
    query_text: &str,
    k: usize,
    filter: Option<Filter>,
) -> Vec<RawHit> {
    let query = Query { text: query_text.into(), k, filter, as_of: None };
    let keyword: Arc<dyn KeywordPort> = Arc::new(NoKeyword);
    let ctx = QueryContext::with_moon(
        query,
        scope.clone(),
        embedder.clone() as Arc<dyn Embedder>,
        storage.clone() as Arc<dyn StoragePort>,
        keyword,
        storage.clone(),
    );
    let retriever = Vector::new("chunks", k).and(Keyword::bm25("chunks", k)).fuse_rrf(k as u32);
    retriever.retrieve(&ctx).await.expect("hybrid recall must not error")
}

// ---------------------------------------------------------------------------
// Assertion helpers
// ---------------------------------------------------------------------------

/// Set of 16-byte ULID keys (matches `RawHit.id`, the decoded Moon vector key).
pub fn id_bytes(ids: &[Ulid]) -> HashSet<Vec<u8>> {
    ids.iter().map(|u| u.to_bytes().to_vec()).collect()
}

/// Build an `Hlc` at the given wall-clock millis (counter/node 0).
pub fn hlc_at(ms: u64) -> Hlc {
    Hlc::from_parts(ms, 0, 0)
}

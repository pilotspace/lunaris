//! ADD task `ft-navigate-filter-gap` (contract v1, milestone
//! claude-code-flagship) — live-Moon proof that a `.filter(...)`'d Navigate
//! recall never leaks a filter-violating hit, not even via BFS expansion
//! from a matching seed.
//!
//! Runs only when `MOON_URL` is set and reachable (skip discipline mirrors
//! `lunaris-storage-moon/tests/navigate_recall.rs`). All seeding goes through
//! the PRODUCTION `atomic_write` path — Built ≠ wired.
//!
//! ```bash
//! MOON_URL=moon://localhost:6380 cargo test -p lunaris-retrieve --test navigate_filter_moon -- --nocapture
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::keyword::KeywordPort;
use lunaris_core::storage::types::{Filter, WriteOp};
use lunaris_core::{Embedder, LunarisError, Scope};
use lunaris_retrieve::{Navigate, Query, QueryContext, Retriever};
use lunaris_storage_moon::MoonStorage;
use serde_json::json;
use ulid::Ulid;

// Match the server's existing 768-d indices (Phase-22 dim guardrail).
const DIM: usize = 768;

async fn connect_or_skip() -> Option<MoonStorage> {
    let Ok(url) = std::env::var("MOON_URL") else {
        eprintln!("MOON_URL unset; SKIP");
        return None;
    };
    match MoonStorage::connect_with_dim(&url, DIM).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("MOON_URL not reachable ({e}); SKIP");
            None
        }
    }
}

fn fresh_scope() -> Scope {
    Scope::new(format!("navf-{}", Ulid::new().to_string().to_lowercase())).expect("valid scope")
}

/// Deterministic 16-byte entity id from a marker byte.
fn eid(marker: u8) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[0] = marker;
    id[15] = marker;
    id
}

/// DIM-d embedding with two controlled components.
fn emb(x: f32, y: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[0] = x;
    v[1] = y;
    v
}

fn node(id: Vec<u8>, name: &str) -> WriteOp {
    WriteOp::GraphNode {
        graph: "lunaris_graph".into(),
        id: id.clone(),
        label: "Person".into(),
        props: json!({
            "id_hex": hex_encode(&id),
            "name": name,
            "type": "Person",
            "aliases": [name.to_lowercase()],
            "confidence": 0.9,
        }),
        index_kind: "entities".into(),
    }
}

fn hex_encode(id: &[u8]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn vector(id: Vec<u8>, name: &str, e: Vec<f32>) -> WriteOp {
    // `content` on the entities FT index becomes "{name} {entity_type}"
    // (atomic.rs::extract_content_for_index) — the filterable TEXT field.
    WriteOp::VectorUpsert {
        index: "entities".into(),
        id,
        embedding: e,
        metadata: json!({"entity_type": "Person", "name": name}),
    }
}

fn edge(src: Vec<u8>, dst: Vec<u8>) -> WriteOp {
    WriteOp::GraphEdge {
        graph: "lunaris_graph".into(),
        src,
        dst,
        rel: "KNOWS".into(),
        props: json!({"confidence": 0.8}),
    }
}

/// Test embedder returning a fixed query-shaped vector so the vector seeds
/// are deterministic relative to the fixture embeddings.
struct FixedEmbedder;

#[async_trait]
impl Embedder for FixedEmbedder {
    fn dim(&self) -> usize {
        DIM
    }
    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        Ok(inputs.iter().map(|_| emb(1.0, 0.01)).collect())
    }
}

fn ctx_with_filter(moon: Arc<MoonStorage>, scope: Scope, filter: Option<Filter>) -> QueryContext {
    let storage: Arc<dyn StoragePort> = moon.clone();
    let keyword: Arc<dyn KeywordPort> = moon;
    let embedder: Arc<dyn Embedder> = Arc::new(FixedEmbedder);
    let mut query = Query::text("alpha");
    query.filter = filter;
    QueryContext::new(query, scope, embedder, storage, keyword)
}

/// §2 scenario 3 — filtered Navigate on live Moon never leaks a foreign hit.
///
/// Fixture: alpha (near the query) --KNOWS--> beta (vector-far, graph-linked).
/// Discriminator FIRST: an unfiltered Navigate DOES surface beta (proves the
/// leak path exists and the red assertion is satisfiable). Then the filtered
/// Navigate must return only content-matching hits — beta never surfaces,
/// not even via BFS expansion from the matching seed alpha.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filtered_navigate_never_leaks_foreign_source_moon() {
    let Some(moon) = connect_or_skip().await else { return };
    let moon = Arc::new(moon);
    let scope = fresh_scope();

    let ops = vec![
        node(eid(1), "alpha"),
        vector(eid(1), "alpha", emb(1.0, 0.0)),
        node(eid(2), "beta"),
        vector(eid(2), "beta", emb(0.0, 10.0)),
        edge(eid(1), eid(2)),
    ];
    moon.atomic_write(&scope, &ops).await.expect("production atomic_write");

    // Discriminator: unfiltered Navigate surfaces the graph-linked beta.
    let op = Navigate::new("entities", 2).with_hops(2).expect("valid hops");
    let ctx = ctx_with_filter(moon.clone(), scope.clone(), None);
    let unfiltered = op.retrieve(&ctx).await.expect("unfiltered navigate");
    let unfiltered_ids: Vec<_> = unfiltered.iter().map(|h| h.id.clone()).collect();
    assert!(
        unfiltered_ids.contains(&eid(2)),
        "discriminator: unfiltered Navigate must surface graph-linked beta; got {unfiltered_ids:?}"
    );

    // The filtered recall: only name==alpha hits may surface. On the
    // entities index this exercises the v1.1 post-filter path (no TAG/NUMERIC
    // fields on entities — metadata-evaluated client-side after over-fetch).
    let filter = Filter::Eq { field: "name".into(), value: json!("alpha") };
    let ctx = ctx_with_filter(moon.clone(), scope.clone(), Some(filter));
    let filtered = op.retrieve(&ctx).await.expect("filtered navigate");
    let filtered_ids: Vec<_> = filtered.iter().map(|h| h.id.clone()).collect();
    assert!(
        !filtered_ids.contains(&eid(2)),
        "filtered Navigate leaked the foreign entity beta (BFS expansion included); got {filtered_ids:?}"
    );
    assert!(
        filtered_ids.contains(&eid(1)),
        "the filter-matching entity alpha must still surface; got {filtered_ids:?}"
    );
}

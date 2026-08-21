//! ADD task `ft-navigate-filter-gap` AMENDMENT v1.1 — live-Moon proof that
//! `vector_search` filters are actually enforced (server-side for the
//! chunks/source TAG subset, client-side post-filter for everything else).
//!
//! Raised from the 2026-07-14 live probe: the pre-v1.1 rendering
//! (`({filter})=>[KNN…]` + `ft_tag_escape`) was SILENTLY DROPPED by Moon's
//! `parse_filter_string` (leading '(' aborts the parse) and, unwrapped,
//! matched nothing (escape backslashes compared as raw bytes).
//!
//! Gated behind `moon-it` + `MOON_URL` like `navigate_recall.rs`.

#![cfg(feature = "moon-it")]

use lunaris_core::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::{Filter, WriteOp};
use lunaris_storage_moon::MoonStorage;
use serde_json::json;
use ulid::Ulid;

mod common;
const DIM: usize = 768;

fn url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".to_string())
}

async fn connect_or_skip() -> Option<MoonStorage> {
    match MoonStorage::connect_with_dim(&url(), DIM).await {
        Ok(s) => Some(s),
        Err(e) => {
            common::note_moon_unreachable(e);
            None
        }
    }
}

fn fresh_scope() -> Scope {
    Scope::new(format!("vf-{}", Ulid::new().to_string().to_lowercase())).expect("valid scope")
}

fn cid(marker: u8) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    id[0] = marker;
    id[15] = marker;
    id
}

fn emb(x: f32, y: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[0] = x;
    v[1] = y;
    v
}

/// A production-shaped chunk VectorUpsert: `source` lands in the FT TAG
/// field AND in the meta JSON (post-filter reads the latter).
fn chunk(id: Vec<u8>, text: &str, source: &str, e: Vec<f32>) -> WriteOp {
    WriteOp::VectorUpsert {
        index: "chunks".into(),
        id,
        embedding: e,
        metadata: json!({"text": text, "source": source}),
    }
}

async fn seed(moon: &MoonStorage, scope: &Scope) {
    let ops = vec![
        chunk(cid(1), "alpha doc", "alpha.md", emb(1.0, 0.0)),
        chunk(cid(2), "beta doc", "beta.md", emb(0.9, 0.1)),
    ];
    moon.atomic_write(scope, &ops).await.expect("production atomic_write");
}

fn ids(hits: &[lunaris_core::storage::types::VectorHit]) -> Vec<Vec<u8>> {
    hits.iter().map(|h| h.id.clone()).collect()
}

/// v1.1 server-renderable subset — Eq{source} on chunks resolves as a raw
/// (unescaped, unparenthesized) `@source:{value}` TAG unit. Both docs sit
/// near the query so the pre-v1.1 silent-drop returns BOTH (red).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_eq_source_tag_enforced_server_side() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = fresh_scope();
    seed(&moon, &scope).await;

    let unfiltered = moon
        .vector_search(&scope, "chunks", &emb(1.0, 0.05), 2, None, None, false)
        .await
        .expect("unfiltered search");
    assert_eq!(unfiltered.len(), 2, "discriminator: both docs reachable at k=2");

    let f = Filter::Eq { field: "source".into(), value: json!("alpha.md") };
    let hits = moon
        .vector_search(&scope, "chunks", &emb(1.0, 0.05), 2, Some(&f), None, false)
        .await
        .expect("filtered search");
    assert_eq!(
        ids(&hits),
        vec![cid(1)],
        "Eq{{source}} must return ONLY the alpha doc; a beta leak means the filter was dropped"
    );
}

/// v1.1 post-filter path — Or is not renderable in Moon's inline grammar;
/// the client-side metadata evaluator must keep BOTH matching docs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_or_post_filter_keeps_all_matching() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = fresh_scope();
    seed(&moon, &scope).await;

    let f = Filter::Or(vec![
        Filter::Eq { field: "source".into(), value: json!("alpha.md") },
        Filter::Eq { field: "source".into(), value: json!("beta.md") },
    ]);
    let hits = moon
        .vector_search(&scope, "chunks", &emb(1.0, 0.05), 2, Some(&f), None, false)
        .await
        .expect("or-filtered search");
    assert_eq!(hits.len(), 2, "Or over both sources must keep both docs; got {:?}", ids(&hits));
}

/// v1.1 post-filter path — StartsWith is not renderable either; the
/// evaluator must enforce the prefix against metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_starts_with_post_filter_excludes_foreign() {
    let Some(moon) = connect_or_skip().await else { return };
    let scope = fresh_scope();
    seed(&moon, &scope).await;

    let f = Filter::StartsWith { field: "source".into(), prefix: "alpha".into() };
    let hits = moon
        .vector_search(&scope, "chunks", &emb(1.0, 0.05), 2, Some(&f), None, false)
        .await
        .expect("startswith-filtered search");
    assert_eq!(
        ids(&hits),
        vec![cid(1)],
        "StartsWith(alpha) must exclude beta.md; a leak means the filter was dropped"
    );
}

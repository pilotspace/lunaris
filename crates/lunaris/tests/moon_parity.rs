//! ADD task `moon-parity-honesty` (contract FROZEN @ v1, 2026-07-14):
//! the Moon backend must KEEP the promises the MCP contract makes.
//!
//! Live deep-test evidence (memory `project_lunaris_mcp_deep_test_findings`):
//! - §2 three identical dedupe-keyed writes minted three episodes (trait-default
//!   fall-through — "SQLite-only idempotency", handle.rs:1156);
//! - §4 scratchpad key `state` was write-OK/read-IMPOSSIBLE (`ERR empty query
//!   after analysis`) and a fresh scope's first read errored `unknown index`.
//!
//! Live gate: LUNARIS_MOON_URL (skipped when unset).
//! RED until the Moon dedupe sidecar + WorkingMemory find-fallback land.

use std::sync::Arc;

use lunaris::{EpisodeBuilder, IngestKind, Lunaris, WorkingMemory};
use lunaris_core::{Scope, StubEmbedder};
use ulid::Ulid;

fn moon_url() -> Option<String> {
    match std::env::var("LUNARIS_MOON_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            eprintln!("skipping: LUNARIS_MOON_URL not set (live-Moon gate)");
            None
        }
    }
}

async fn open_moon(url: &str) -> Arc<Lunaris> {
    Arc::new(
        Lunaris::open_with_embedder(url, Arc::new(StubEmbedder::new(768)))
            .await
            .expect("open live Moon"),
    )
}

fn fresh_scope(tag: &str) -> Scope {
    Scope::new(format!("parity-{tag}-{}", Ulid::new().to_string().to_lowercase())).unwrap()
}

/// §2 "dedupe key is idempotent on Moon": the second identical-keyed ingest
/// returns `IngestKind::Duplicate` carrying the FIRST call's Lsn.
#[tokio::test]
async fn dedupe_key_idempotent_on_moon() {
    let Some(url) = moon_url() else { return };
    let engine = open_moon(&url).await;
    let scoped = engine.scoped(fresh_scope("dedupe"));

    let (first_lsn, first_kind) = scoped
        .ingest_idempotent(
            EpisodeBuilder::new("dedupe/probe", "the saffron gate opens at dusk"),
            "parity-dedupe-001",
        )
        .await
        .expect("first ingest");
    assert!(matches!(first_kind, IngestKind::Fresh), "first keyed write must be Fresh");

    let (second_lsn, second_kind) = scoped
        .ingest_idempotent(
            EpisodeBuilder::new("dedupe/probe", "the saffron gate opens at dusk"),
            "parity-dedupe-001",
        )
        .await
        .expect("second ingest");
    assert!(
        matches!(second_kind, IngestKind::Duplicate(prior) if prior == first_lsn),
        "second identical-keyed write must be Duplicate(first LSN); got {second_kind:?}"
    );
    assert_eq!(second_lsn, first_lsn, "duplicate must return the prior LSN, not a new one");
}

/// §2 "dedupe keys are scope-isolated": the same raw key under a different
/// scope is a fresh write, never a cross-scope duplicate.
#[tokio::test]
async fn dedupe_key_scope_isolated_on_moon() {
    let Some(url) = moon_url() else { return };
    let engine = open_moon(&url).await;

    let (_, kind_a) = engine
        .scoped(fresh_scope("iso1"))
        .ingest_idempotent(EpisodeBuilder::new("dedupe/a", "isolated alpha"), "parity-shared-key")
        .await
        .expect("scope A ingest");
    let (_, kind_b) = engine
        .scoped(fresh_scope("iso2"))
        .ingest_idempotent(EpisodeBuilder::new("dedupe/b", "isolated beta"), "parity-shared-key")
        .await
        .expect("scope B ingest");

    assert!(matches!(kind_a, IngestKind::Fresh));
    assert!(
        matches!(kind_b, IngestKind::Fresh),
        "the same raw key under another scope must be Fresh, got {kind_b:?}"
    );
}

/// §2 "stopword key reads back on Moon": Moon's FT analyzer reduces `state`
/// to an empty query — the exact-key read must survive that.
#[tokio::test]
async fn scratchpad_stopword_key_reads_back_moon() {
    let Some(url) = moon_url() else { return };
    let engine = open_moon(&url).await;
    let scope = fresh_scope("stopword");
    let wm = WorkingMemory::new(engine.clone(), scope, "scratchpad/");

    let value = serde_json::json!({"phase": "verify", "count": 7});
    wm.write("state", value.clone()).await.expect("write");

    let read = wm.read("state").await.expect("read must not error on an analyzer-eaten key");
    assert_eq!(read, Some(value), "stopword-like key must round-trip verbatim");
}

/// §2 "fresh-scope read returns none": before ANY ingest a scope has no FT
/// index — the read must be `Ok(None)`, not `unknown index`.
#[tokio::test]
async fn scratchpad_read_fresh_scope_returns_none_moon() {
    let Some(url) = moon_url() else { return };
    let engine = open_moon(&url).await;
    let wm = WorkingMemory::new(engine, fresh_scope("virgin"), "scratchpad/");

    let read = wm
        .read("anything")
        .await
        .expect("read on a brand-new scope must not surface 'unknown index'");
    assert_eq!(read, None);
}

/// F1 — a brand-new scope's FIRST recall must be `Ok(vec![])`, not an error.
///
/// Moon creates a scope's FT index lazily, on first write. A scope that has
/// never been ingested into therefore has no index, and `FT.SEARCH` answers
/// `unknown index`. `WorkingMemory::read` already handles that
/// (`working_memory.rs::is_unknown_index`); the recall path did not, so the
/// very first thing a new agent does — ask what it remembers, before it has
/// remembered anything — was an error rather than an empty list.
///
/// That is the worst possible place for it: it cannot be hit by an existing
/// deployment, only by a new one, so it survives every amount of production
/// traffic. It surfaced from the Python SDK's `test_cross_scope_isolation`,
/// which recalls under a scope it deliberately never writes to (ledger F1).
///
/// Empty is the honest answer. "I have no index for you" is an implementation
/// detail of lazy creation, and a caller cannot act on it.
#[tokio::test]
async fn recall_on_a_brand_new_scope_returns_no_hits_moon() {
    let Some(url) = moon_url() else { return };
    let engine = open_moon(&url).await;
    let scoped = engine.scoped(fresh_scope("virgin-recall"));

    let hits = scoped
        .recall(lunaris_retrieve::Query::text("anything at all"))
        .await
        .expect("recall on a brand-new scope must not surface 'unknown index'");
    assert!(hits.is_empty(), "a scope with no writes cannot have hits, got {hits:?}");
}

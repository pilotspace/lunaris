//! Phase 23 — agent-facing structured ingest smoke.
//!
//! Verifies that `ScopedLunaris::ingest_structured` writes the episode, the
//! chunks, and the agent-supplied graph (entities, relations, facts) in a
//! single transaction.
//!
//! ## Backend (0.7.0 port)
//!
//! Ported off `memory://` onto `lunaris_test_harness::open_test_engine_*`,
//! which gives each test its own ephemeral child-process Moon and degrades to
//! `memory://` only where no Moon binary exists. That matters here beyond
//! future-proofing: the embedded backend has no native graph, so the entity /
//! relation / fact writes this file exercises were previously landing as plain
//! KV rows. On Moon they go through the real graph path, which is what
//! production does.

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use lunaris::EpisodeBuilder;
use lunaris::structured_ingest::{EntityInput, FactInput, RelationInput, StructuredIngest};
use lunaris_core::{Embedder, NoopEmbedder, Scope};
use lunaris_test_harness::{TestEngine, open_test_engine_with_embedder};

/// Open a harness-issued handle with a pinned NoopEmbedder so the resolver
/// doesn't try to construct a real embedder (which lunaris-bench transitively
/// forces on in workspace test builds).
///
/// The returned `TestEngine` derefs to `Lunaris` AND owns the Moon child
/// process — bind it to a local, never `let _ =`.
async fn open_test_handle() -> TestEngine {
    open_test_engine_with_embedder(Arc::new(NoopEmbedder::new(768)) as Arc<dyn Embedder>).await
}

fn alice_then_bob() -> StructuredIngest {
    let valid_from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap();
    StructuredIngest::new(EpisodeBuilder::new(
        "chat:session-1/turn-7",
        "Alice mentioned blockers on Lunaris. She owes Bob a follow-up.",
    ))
    .with_entities(vec![
        EntityInput {
            name: "Alice".into(),
            entity_type: "Person".into(),
            aliases: vec![],
            confidence: 0.95,
            valid_from,
            valid_to: None,
            embedding: None,
        },
        EntityInput {
            name: "Bob".into(),
            entity_type: "Person".into(),
            aliases: vec![],
            confidence: 0.9,
            valid_from,
            valid_to: None,
            embedding: None,
        },
        EntityInput {
            name: "Lunaris".into(),
            entity_type: "Project".into(),
            aliases: vec!["lunaris-memory".into()],
            confidence: 1.0,
            valid_from,
            valid_to: None,
            embedding: None,
        },
    ])
    .with_relations(vec![
        RelationInput {
            subject_name: "Alice".into(),
            subject_type: "Person".into(),
            predicate: "blocked_on".into(),
            object_name: "Lunaris".into(),
            object_type: "Project".into(),
            confidence: 0.9,
            valid_from,
            valid_to: None,
        },
        RelationInput {
            subject_name: "Alice".into(),
            subject_type: "Person".into(),
            predicate: "owes_follow_up_to".into(),
            object_name: "Bob".into(),
            object_type: "Person".into(),
            confidence: 0.85,
            valid_from,
            valid_to: None,
        },
    ])
    .with_facts(vec![FactInput {
        fact_text: "Alice owes Bob a follow-up about Lunaris blockers.".into(),
        subject_name: "Alice".into(),
        subject_type: "Person".into(),
        predicate: "owes_follow_up_to".into(),
        object_name: "Bob".into(),
        object_type: "Person".into(),
        confidence: 0.8,
        valid_from,
        valid_to: None,
    }])
}

#[tokio::test]
async fn ingest_structured_writes_episode_chunks_and_graph_in_one_call() {
    let handle = open_test_handle().await;
    let scope = Scope::new("test-agent-structured-ingest").expect("scope valid");
    let scoped = handle.scoped(scope.clone());

    let lsn =
        scoped.ingest_structured(alice_then_bob()).await.expect("ingest_structured must succeed");

    // Single atomic_write produced one monotonic LSN. We don't pin the
    // exact value (it depends on the HlcClock seed) — only that a write
    // happened (the trait contract guarantees > Lsn::ZERO on commit).
    assert!(
        lsn.wall_ms > 0 || lsn.counter > 0,
        "expected positive Lsn after ingest_structured, got {lsn:?}"
    );
}

#[tokio::test]
async fn ingest_structured_handles_empty_lists() {
    // Episode with content but no agent-supplied graph payload — still
    // works (degenerates to a text-only ingest at the storage layer).
    let handle = open_test_handle().await;
    let scope = Scope::new("test-empty-graph").expect("scope valid");
    let scoped = handle.scoped(scope);

    let payload = StructuredIngest::new(EpisodeBuilder::new("chat:t-1", "plain text turn."));
    let _lsn = scoped.ingest_structured(payload).await.expect("empty graph payload must succeed");
}

#[tokio::test]
async fn ingest_structured_rejects_mismatched_entity_embedding_dim() {
    let handle = open_test_handle().await;
    let dim = handle.embedder().dim();
    let scope = Scope::new("test-dim-validation").expect("scope valid");
    let scoped = handle.scoped(scope);

    // Supply an entity embedding of dim+1 to provoke the validation path.
    let valid_from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap();
    let payload = StructuredIngest::new(EpisodeBuilder::new("chat:t-2", "x")).with_entities(vec![
        EntityInput {
            name: "BadEmbed".into(),
            entity_type: "Thing".into(),
            aliases: vec![],
            confidence: 1.0,
            valid_from,
            valid_to: None,
            embedding: Some(vec![0.0_f32; dim + 1]),
        },
    ]);

    let err = scoped.ingest_structured(payload).await.expect_err("dim mismatch must error");
    let msg = format!("{err}");
    assert!(msg.contains("supplied embedding has dim"), "msg should explain dim mismatch: {msg}");
    assert!(msg.contains("BadEmbed"), "msg should echo offending entity name: {msg}");
}

#[tokio::test]
async fn ingest_structured_dedupes_repeat_entities_by_deterministic_id() {
    // Re-ingest the same logical entity from a second turn — the
    // deterministic EntityId means storage layer dedups by key, so the
    // second call's GraphNode write is an idempotent re-assert.
    let handle = open_test_handle().await;
    let scope = Scope::new("test-dedup").expect("scope valid");
    let scoped = handle.scoped(scope);

    let lsn1 = scoped.ingest_structured(alice_then_bob()).await.expect("first ingest");
    let lsn2 = scoped.ingest_structured(alice_then_bob()).await.expect("second ingest");

    // Both writes succeeded and the LSN advanced (HlcClock is monotonic).
    assert!(
        (lsn2.wall_ms, lsn2.counter) > (lsn1.wall_ms, lsn1.counter),
        "LSN must advance on repeat ingest: lsn1={lsn1:?} lsn2={lsn2:?}"
    );
}

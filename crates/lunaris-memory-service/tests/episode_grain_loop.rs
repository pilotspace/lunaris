//! engram id-space fix (2026-07-19 live finding) — end-to-end episode-grain
//! reinforcement loop.
//!
//! The live repro this pins: `memory.recall` returned CHUNK ULIDs in the
//! `episode_id` field, `memory.feedback` wrote the activation row against
//! whatever id it was handed, and the two ledger consumers keyed by
//! DIFFERENT id spaces — recall-boost by chunk id (worked by accident),
//! `memory.dream_agenda` by episode id (silently dropped the candidate).
//! Result: a user voting on ids recall handed them could never grow a dream
//! agenda.
//!
//! This test drives the whole loop through the public service handlers over an
//! ephemeral Moon + `StubEmbedder` and fails unless ALL THREE seams agree on
//! episode-grain:
//! 1. recall's wire `episode_id` is the parent episode ULID;
//! 2. feedback on that id lands an episode-keyed ledger row;
//! 3. the next recall is boosted by that row AND `memory.dream_agenda`
//!    surfaces the episode as a candidate cluster.

use std::sync::Arc;

use lunaris::EpisodeBuilder;
use lunaris_core::{Scope, StubEmbedder};
use lunaris_memory_service::dream_agenda::{self, DreamAgendaParams};
use lunaris_memory_service::feedback::{self, FeedbackParams, Sentiment};
use lunaris_memory_service::recall::{self, RecallParams};
use lunaris_test_harness::{TestEngine, open_test_engine_with_embedder};

/// Ported off `memory://` (0.7.0 prerequisite) onto a harness-issued ephemeral
/// Moon, falling back to `memory://` only where no Moon binary exists.
///
/// This is the highest-value backend swap in the file: the loop under test is
/// recall ranking → activation ledger → boosted re-recall, and on the embedded
/// backend that ran over SQLite brute-force scoring. On Moon it runs over the
/// real FT vector index — the same path production takes.
///
/// `TestEngine` derefs to `Lunaris`, so every `&lunaris` call site below
/// coerces unchanged; the binding must stay alive because it owns the Moon
/// child process.
async fn make_engine(scope_name: &str) -> (TestEngine, Scope) {
    let embedder = Arc::new(StubEmbedder::new(768));
    let lunaris = open_test_engine_with_embedder(embedder).await;
    let scope = Scope::new(scope_name).unwrap();
    (lunaris, scope)
}

fn recall_params(query: &str) -> RecallParams {
    RecallParams { query: query.into(), k: 5, filters: None, as_of: None, raw: false }
}

#[tokio::test]
async fn feedback_on_recall_returned_id_boosts_recall_and_feeds_dream_agenda() {
    let (lunaris, scope) = make_engine("test-episode-grain-loop").await;
    let scoped = lunaris.scoped(scope.clone());
    scoped
        .ingest(EpisodeBuilder::new(
            "test/lesson",
            "when injection goes quiet check the contextd process age and socket first",
        ))
        .await
        .unwrap();

    // 1. Recall — capture the id the wire hands to the model.
    let before =
        recall::handle(&lunaris, &scope, recall_params("contextd injection quiet")).await.unwrap();
    let hit = before.hits.first().expect("recall must return the ingested episode");
    let voted_id = hit.episode_id.clone();
    let score_before = hit.score;

    // 2. Feedback — vote on EXACTLY the id recall returned (the realistic
    //    caller behavior; no out-of-band id lookups allowed here).
    let fb = feedback::handle(
        &lunaris,
        &scope,
        FeedbackParams {
            memory_id: voted_id.clone(),
            sentiment: Sentiment::Positive,
            reason: "durable operational lesson worth reinforcing".into(),
            dedupe_key: None,
        },
    )
    .await
    .unwrap();
    assert!(fb.activation_applied, "the ledger signal must apply");

    // 3a. The reinforced episode's next recall is boosted — its score must
    //     strictly increase vs the unboosted pass.
    let after =
        recall::handle(&lunaris, &scope, recall_params("contextd injection quiet")).await.unwrap();
    let boosted = after
        .hits
        .iter()
        .find(|h| h.episode_id == voted_id)
        .expect("the reinforced episode must still be recalled");
    assert!(
        boosted.score > score_before,
        "episode-keyed ledger row must boost recall (before={score_before}, after={})",
        boosted.score
    );

    // 3b. The dream agenda must surface the reinforced episode as a
    //     candidate — the exact seam that was silently empty in the live
    //     repro.
    let agenda = dream_agenda::handle(
        &lunaris,
        &scope,
        DreamAgendaParams { limit: None, min_cluster_size: None, max_activation: None },
    )
    .await
    .unwrap();
    assert!(
        agenda.clusters.iter().any(|c| c.member_episode_ids.contains(&voted_id)),
        "dream agenda must contain the episode voted on via a recall-returned id; got {:?}",
        agenda.clusters
    );
}

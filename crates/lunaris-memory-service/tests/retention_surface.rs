//! Wave 6 / R1 — the retention surface an operator can actually reach.
//!
//! The engine shipped `ScopedLunaris::{retention_policy, set_retention_policy,
//! enforce_retention}` in W4.6 and, until this landed, they had **no caller
//! outside `crates/lunaris/tests/retention_policy.rs`**. `lunaris-py`,
//! `lunaris-ts`, `lunaris-mcp` and `lunaris-server` between them contained
//! zero occurrences of the word "retention".
//!
//! That made the engine's deliberate no-scheduler stance ("the MCP / hook /
//! HTTP surfaces can expose it; the scheduling belongs to whoever owns the
//! deployment") into an unredeemed promise: no surface exposed it, so on a
//! default install nothing cleaned up memories, and nothing could be made to.
//!
//! These run the PRODUCTION handlers — `lunaris_memory_service::retention` is
//! what `MemoryRequest::{Retention,RetentionEnforce}` dispatches to — against
//! a real store, so a wiring break shows up here rather than in a doc.

use std::sync::Arc;

use lunaris_core::{Scope, StubEmbedder};
use lunaris_memory_service::ingest::{self, IngestParams};
use lunaris_memory_service::retention::{self, RetentionEnforceParams, RetentionParams};
use lunaris_test_harness::{TestEngine, open_test_engine_with_embedder};

async fn make_engine(scope_name: &str) -> (TestEngine, Scope) {
    let embedder = Arc::new(StubEmbedder::new(768));
    let lunaris = open_test_engine_with_embedder(embedder).await;
    let scope = Scope::new(scope_name).unwrap();
    (lunaris, scope)
}

async fn seed(engine: &TestEngine, scope: &Scope, n: usize) {
    for i in 0..n {
        ingest::handle(
            engine,
            scope,
            IngestParams {
                source: format!("w6ret/{i}"),
                content: format!("seeded record {i}"),
                t_ref: None,
                metadata: None,
                dedupe_key: None,
            },
        )
        .await
        .expect("seed ingest");
    }
}

/// `RecallParams` has no `Default`; spell the read once so every assertion
/// below queries the store identically.
fn recall_params(query: &str) -> lunaris_memory_service::recall::RecallParams {
    lunaris_memory_service::recall::RecallParams {
        query: query.to_owned(),
        k: 10,
        filters: None,
        as_of: None,
        raw: false,
    }
}

/// A scope with no policy must report that fact distinctly, on BOTH ops.
///
/// This is the state every Lunaris scope is in by default, and the one an
/// operator most needs told apart from "a sweep ran and found nothing" — the
/// two look identical in a `removed: 0` and mean opposite things.
#[tokio::test]
async fn an_unconfigured_scope_says_so_on_both_ops() {
    let (engine, scope) = make_engine("w6ret_unset").await;

    let read = retention::handle(&engine, &scope, RetentionParams::default())
        .await
        .expect("read a scope with no policy");
    assert_eq!(read.status, "read");
    assert!(!read.configured, "an untouched scope reported a retention policy");
    assert!(read.max_age_ms.is_none());
    assert!(!read.hard);

    let swept =
        retention::handle_enforce(&engine, &scope, RetentionEnforceParams { dry_run: false })
            .await
            .expect("enforce on a scope with no policy must succeed");
    assert_eq!(
        swept.status, "no_policy",
        "an unconfigured sweep reported `{}`, which reads as a sweep that ran",
        swept.status
    );
    assert!(!swept.configured);
    assert_eq!((swept.matched, swept.removed), (0, 0));
    assert!(swept.cutoff_ms.is_none(), "a scope with no policy reported a cutoff");
}

/// Set → read round-trip, through the handlers a tool call reaches.
///
/// The read is the discriminating half: a `set` that returned the right
/// response while writing nothing would pass a set-only assertion.
#[tokio::test]
async fn a_policy_set_through_the_tool_is_readable_through_the_tool() {
    let (engine, scope) = make_engine("w6ret_roundtrip").await;

    let set = retention::handle(
        &engine,
        &scope,
        RetentionParams { max_age_ms: Some(86_400_000), hard: None },
    )
    .await
    .expect("set");
    assert_eq!(set.status, "set");
    assert_eq!(set.max_age_ms, Some(86_400_000));
    assert!(!set.hard, "a policy set without `hard` came back HARD");

    let read =
        retention::handle(&engine, &scope, RetentionParams::default()).await.expect("read back");
    assert_eq!(read.status, "read");
    assert!(read.configured);
    assert_eq!(
        read.max_age_ms,
        Some(86_400_000),
        "the policy did not survive the write — read back {:?}",
        read.max_age_ms
    );
}

/// The default is a PREVIEW, proven by what is left in the store afterwards.
///
/// Asserting on the response's `status` alone would pass against a handler
/// that swept and mislabelled the result, which is the failure that matters.
/// So this counts the episodes a recall can still see.
#[tokio::test]
async fn enforce_previews_by_default_and_leaves_the_store_intact() {
    use lunaris_memory_service::recall;

    let (engine, scope) = make_engine("w6ret_preview").await;
    seed(&engine, &scope, 3).await;

    retention::handle(&engine, &scope, RetentionParams { max_age_ms: Some(0), hard: None })
        .await
        .expect("set policy");

    // Omitting dry_run entirely is the case an LLM will produce most often.
    let params: RetentionEnforceParams = serde_json::from_str("{}").expect("empty enforce params");
    let preview = retention::handle_enforce(&engine, &scope, params).await.expect("preview");

    assert_eq!(preview.status, "preview", "an unqualified enforce reported `{}`", preview.status);
    assert!(preview.dry_run);
    assert!(preview.configured);
    assert_eq!(preview.matched, 3, "preview must report what a sweep would take");
    assert_eq!(preview.removed, 0, "a PREVIEW removed {} episodes", preview.removed);
    assert!(preview.cutoff_ms.is_some(), "preview reported no cutoff");

    let after = recall::handle(&engine, &scope, recall_params("seeded record"))
        .await
        .expect("recall after preview");
    assert_eq!(
        after.hits.len(),
        3,
        "the preview removed data: recall sees {} of 3 seeded episodes",
        after.hits.len()
    );
}

/// `dry_run: false` actually sweeps — otherwise the preview default would be
/// a surface with no way through it, which is the same bug wearing the
/// opposite sign.
#[tokio::test]
async fn an_explicit_commit_sweeps_and_recall_stops_seeing_it() {
    use lunaris_memory_service::recall;

    let (engine, scope) = make_engine("w6ret_commit").await;
    seed(&engine, &scope, 3).await;

    retention::handle(&engine, &scope, RetentionParams { max_age_ms: Some(0), hard: None })
        .await
        .expect("set policy");

    let swept =
        retention::handle_enforce(&engine, &scope, RetentionEnforceParams { dry_run: false })
            .await
            .expect("sweep");
    assert_eq!(swept.status, "swept", "a committing enforce reported `{}`", swept.status);
    assert!(!swept.dry_run);
    assert_eq!(swept.matched, 3);
    assert_eq!(swept.removed, 3, "a committing sweep reported {} removed", swept.removed);

    let after = recall::handle(&engine, &scope, recall_params("seeded record"))
        .await
        .expect("recall after sweep");
    assert!(
        after.hits.is_empty(),
        "recall still returns {} swept episodes — the soft-delete sys-gate did not apply",
        after.hits.len()
    );
}

/// A `hard` policy PREVIEWED is still a preview.
///
/// The engine's preview branch never mints a D-21 confirmation token, so
/// there must be no route through this tool where omitting `dry_run` on a
/// hard policy destroys anything. A regression that routed hard policies
/// straight to `enforce_retention` would be unrecoverable by construction,
/// which is why it gets its own test rather than a note.
#[tokio::test]
async fn a_hard_policy_previewed_still_takes_nothing() {
    use lunaris_memory_service::recall;

    let (engine, scope) = make_engine("w6ret_hardpv").await;
    seed(&engine, &scope, 2).await;

    let set = retention::handle(
        &engine,
        &scope,
        RetentionParams { max_age_ms: Some(0), hard: Some(true) },
    )
    .await
    .expect("set hard policy");
    assert!(set.hard, "a policy set with hard=true came back soft");

    let params: RetentionEnforceParams = serde_json::from_str("{}").expect("params");
    let preview = retention::handle_enforce(&engine, &scope, params).await.expect("preview");
    assert_eq!(preview.status, "preview");
    assert!(preview.hard, "the preview lost the policy's hard flag");
    assert_eq!(preview.removed, 0, "a HARD policy previewed and removed {}", preview.removed);

    let after =
        recall::handle(&engine, &scope, recall_params("seeded record")).await.expect("recall");
    assert_eq!(
        after.hits.len(),
        2,
        "a hard policy previewed and destroyed data: {} of 2 left",
        after.hits.len()
    );
}

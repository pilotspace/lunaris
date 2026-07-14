//! ADD task `forget-scope-routing` (contract FROZEN @ v1, 2026-07-14):
//! `ScopedLunaris::forget` must scan/stamp under the BOUND scope — the live
//! deep test (memory `project_lunaris_mcp_deep_test_findings` §1) proved the
//! shipped shim returns `removed: 0` for BOTH prefix and exact-ULID targets
//! under any real scope because it delegates to the deprecated
//! `Scope::dev()`-hard-coded pipeline.
//!
//! Live gate: LUNARIS_TEST_MOON_URL (moon-it pattern: skipped when unset).
//! The hard-without-token rejection runs ungated on `memory://`.
//!
//! RED until the scoped forget pipeline lands in
//! `crates/lunaris/src/forget.rs` + `crates/lunaris/src/handle.rs`.

use std::sync::Arc;

use lunaris::{EpisodeBuilder, ForgetTarget, Lunaris, ScopeSpec};
use lunaris_core::{Scope, StubEmbedder};
use lunaris_retrieve::Query;
use ulid::Ulid;

fn moon_url() -> Option<String> {
    match std::env::var("LUNARIS_TEST_MOON_URL") {
        Ok(u) if !u.is_empty() => Some(u),
        _ => {
            eprintln!("skipping: LUNARIS_TEST_MOON_URL not set (live-Moon gate)");
            None
        }
    }
}

async fn open_moon(url: &str) -> Lunaris {
    Lunaris::open_with_embedder(url, Arc::new(StubEmbedder::new(768)))
        .await
        .expect("open live Moon")
}

fn fresh_scope(tag: &str) -> Scope {
    Scope::new(format!("forget-it-{tag}-{}", Ulid::new().to_string().to_lowercase())).unwrap()
}

/// True when any recall hit's provenance points at `episode_id`.
fn recalls_episode(hits: &[lunaris_retrieve::Hit], episode_id: Ulid) -> bool {
    let bytes = episode_id.to_bytes().to_vec();
    hits.iter().any(|h| h.episode_id == bytes)
}

/// §2 "forget by episode id removes it from recall": ingest → recall hits →
/// forget(Id) → rows_written == 1 → recall misses.
#[tokio::test]
async fn forget_id_removes_from_recall_moon() {
    let Some(url) = moon_url() else { return };
    let engine = open_moon(&url).await;
    let scope = fresh_scope("id");
    let scoped = engine.scoped(scope.clone());

    let ep_id = Ulid::new();
    scoped
        .ingest(
            EpisodeBuilder::new("wipe-me/solo", "the cobalt beacon flashes every 17 seconds")
                .id(ep_id),
        )
        .await
        .expect("ingest");

    let pre = scoped.recall(Query::text("cobalt beacon flash interval")).await.expect("recall");
    assert!(recalls_episode(&pre, ep_id), "pre-forget recall must surface the episode: {pre:?}");

    let receipt = scoped.forget(ForgetTarget::Id(ep_id)).await.expect("forget");
    assert_eq!(receipt.rows_written, 1, "forget(Id) must stamp exactly the one episode row");

    let post = scoped.recall(Query::text("cobalt beacon flash interval")).await.expect("recall");
    assert!(
        !recalls_episode(&post, ep_id),
        "post-forget recall must NOT surface the forgotten episode: {post:?}"
    );
}

/// §2 "forget by source prefix removes all matches": three wipe-me/* episodes
/// stamped, keep/x survives and still recalls.
#[tokio::test]
async fn forget_prefix_removes_all_matches_moon() {
    let Some(url) = moon_url() else { return };
    let engine = open_moon(&url).await;
    let scope = fresh_scope("prefix");
    let scoped = engine.scoped(scope.clone());

    let mut wipe_ids = Vec::new();
    for (name, text) in [
        ("wipe-me/a", "amber relay alpha note"),
        ("wipe-me/b", "amber relay beta note"),
        ("wipe-me/c", "amber relay gamma note"),
    ] {
        let id = Ulid::new();
        wipe_ids.push(id);
        scoped.ingest(EpisodeBuilder::new(name, text).id(id)).await.expect("ingest wipe");
    }
    let keep_id = Ulid::new();
    scoped
        .ingest(EpisodeBuilder::new("keep/x", "the violet antenna hums at dawn").id(keep_id))
        .await
        .expect("ingest keep");

    let receipt = scoped
        .forget(ForgetTarget::Scope(ScopeSpec::BySource("wipe-me/".into())))
        .await
        .expect("forget");
    assert_eq!(receipt.rows_written, 3, "all three wipe-me/* episodes must be stamped");

    let post = scoped.recall(Query::text("violet antenna hum")).await.expect("recall");
    assert!(recalls_episode(&post, keep_id), "keep/x must still recall: {post:?}");
    for id in wipe_ids {
        assert!(!recalls_episode(&post, id), "wiped episode {id} must not recall");
    }
}

/// §2 "cross-scope isolation": forget under scope A never sees scope B's rows.
#[tokio::test]
async fn forget_cross_scope_isolated_moon() {
    let Some(url) = moon_url() else { return };
    let engine = open_moon(&url).await;
    let scope_a = fresh_scope("iso-a");
    let scope_b = fresh_scope("iso-b");

    let b_id = Ulid::new();
    engine
        .scoped(scope_b.clone())
        .ingest(EpisodeBuilder::new("wipe-me/z", "the teal lighthouse rotates twice").id(b_id))
        .await
        .expect("ingest under B");

    let receipt = engine
        .scoped(scope_a.clone())
        .forget(ForgetTarget::Scope(ScopeSpec::BySource("wipe-me/".into())))
        .await
        .expect("forget under A");
    assert_eq!(receipt.rows_written, 0, "scope A forget must not stamp scope B rows");

    let post = engine
        .scoped(scope_b)
        .recall(Query::text("teal lighthouse rotation"))
        .await
        .expect("recall");
    assert!(recalls_episode(&post, b_id), "scope B episode must survive scope A's forget");
}

/// §2 "dry run previews without writing": preview == true, rows_written == 0,
/// the episode still recalls afterwards.
#[tokio::test]
async fn forget_dry_run_previews_without_writing_moon() {
    let Some(url) = moon_url() else { return };
    let engine = open_moon(&url).await;
    let scope = fresh_scope("dry");
    let scoped = engine.scoped(scope.clone());

    let ep_id = Ulid::new();
    scoped
        .ingest(EpisodeBuilder::new("wipe-me/dry", "the crimson buoy bobs at noon").id(ep_id))
        .await
        .expect("ingest");

    let receipt = scoped
        .forget(ForgetTarget::Scope(ScopeSpec::BySource("wipe-me/".into())).dry_run())
        .await
        .expect("dry-run forget");
    assert!(receipt.preview, "dry_run receipt must carry preview=true");
    assert_eq!(receipt.rows_written, 0, "dry_run must write nothing");

    let post = scoped.recall(Query::text("crimson buoy noon")).await.expect("recall");
    assert!(recalls_episode(&post, ep_id), "dry_run must leave the episode recallable");
}

/// §2 "hard delete without token rejected" — ungated (`memory://`), the
/// SCOPED path must keep the D-21 safety rail.
#[tokio::test]
async fn forget_hard_without_token_rejected() {
    let engine = Lunaris::open_with_embedder("memory://", Arc::new(StubEmbedder::new(768)))
        .await
        .expect("open embedded");
    let scoped = engine.scoped(Scope::new("hard-no-token").unwrap());

    let err = scoped
        .forget(ForgetTarget::Id(Ulid::new()).hard())
        .await
        .expect_err("hard without token must be rejected");
    assert!(
        format!("{err}").contains("confirmation"),
        "error must name the missing confirmation token, got: {err}"
    );
}

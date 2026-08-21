//! ADD task `forget-scope-routing` (contract FROZEN @ v1, 2026-07-14):
//! `ScopedLunaris::forget` must scan/stamp under the BOUND scope — the live
//! deep test (memory `project_lunaris_mcp_deep_test_findings` §1) proved the
//! shipped shim returns `removed: 0` for BOTH prefix and exact-ULID targets
//! under any real scope because it delegates to the deprecated
//! `Scope::dev()`-hard-coded pipeline.
//!
//! ## Backend (0.7.0 port)
//!
//! Previously gated on `LUNARIS_TEST_MOON_URL`, which nothing in CI ever set —
//! so all four Moon assertions below were *skipped on every run*, and only the
//! `memory://` rejection test actually executed. They now open a harness-issued
//! ephemeral Moon and run whenever a Moon binary is present.
//!
//! The `LUNARIS_TEST_MOON_URL` escape hatch is deliberately NOT carried over.
//! This file calls `forget`; pointing it at an operator's live store was a
//! footgun with no upside now that a disposable Moon costs ~3 ms.
//!
//! RED until the scoped forget pipeline lands in
//! `crates/lunaris/src/forget.rs` + `crates/lunaris/src/handle.rs`.

use std::sync::Arc;

use lunaris::{EpisodeBuilder, ForgetTarget, ScopeSpec};
use lunaris_core::{Scope, StubEmbedder};
use lunaris_retrieve::Query;
use lunaris_test_harness::{TestEngine, open_test_engine_with_embedder};
use ulid::Ulid;

/// A disposable Moon-backed engine.
///
/// Through 0.6.x this returned `Option` and printed a skip line when no Moon
/// binary resolved. 0.7.0 removed the substrate that skip existed to fall back
/// to, so the harness itself now fails loudly with build instructions and this
/// helper has nothing left to decide.
///
/// The returned `TestEngine` owns the Moon child process and derefs to
/// `Lunaris`; hold it for the whole test.
async fn open_moon() -> TestEngine {
    open_test_engine_with_embedder(Arc::new(StubEmbedder::new(768))).await
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
/// forget(Id) → rows_written > 1 → recall misses.
///
/// `> 1`, not `== 1`: W1.4 made forget sweep the episode's chunk rows too, and
/// the discriminating form is the one that goes red if that sweep is removed.
#[tokio::test]
async fn forget_id_removes_from_recall_moon() {
    let engine = open_moon().await;
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
    // W1.4 changed what this counts. A forget now stamps the episode row AND
    // its chunk rows, so the receipt reports ROWS — which is what the field is
    // named — instead of episodes. `> 1` is the discriminating form: it is the
    // assertion that fails if the chunk sweep is ever removed, and it does not
    // go brittle when a fixture's chunk count changes.
    assert!(
        receipt.rows_written > 1,
        "forget(Id) stamped {} row(s) — the episode only. Its chunks keep \
         bt.sys.1 = None, and recall stays clean solely because hydrate drops \
         chunks under a sys-closed parent; delete that parent instead (a HARD \
         forget) and the content comes back.",
        receipt.rows_written
    );

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
    let engine = open_moon().await;
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
    // Three episodes plus their chunks — see the note on the Id test above.
    assert!(
        receipt.rows_written > 3,
        "a prefix forget stamped {} row(s), which is the three episode rows and \
         none of their chunks",
        receipt.rows_written
    );

    let post = scoped.recall(Query::text("violet antenna hum")).await.expect("recall");
    assert!(recalls_episode(&post, keep_id), "keep/x must still recall: {post:?}");
    for id in wipe_ids {
        assert!(!recalls_episode(&post, id), "wiped episode {id} must not recall");
    }
}

/// §2 "cross-scope isolation": forget under scope A never sees scope B's rows.
#[tokio::test]
async fn forget_cross_scope_isolated_moon() {
    let engine = open_moon().await;
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
    let engine = open_moon().await;
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

/// §2 "hard delete without token rejected" — the SCOPED path must keep the
/// D-21 safety rail.
///
/// Backend-agnostic on purpose: the rejection fires before any storage call,
/// so this one runs under the default policy (Moon when available, `memory://`
/// otherwise) and needs no skip gate.
#[tokio::test]
async fn forget_hard_without_token_rejected() {
    let engine = open_test_engine_with_embedder(Arc::new(StubEmbedder::new(768))).await;
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

// ---------------------------------------------------------------------------
// W1.4 — a HARD forget leaves the content recallable.
//
// The sibling `forget_id_removes_from_recall_moon` above passes, and it is not
// vacuous: it asserts the episode recalls before the forget and not after. But
// it exercises the SOFT path, and the soft path only appears to work.
//
// `scan_matches_scoped` walks one prefix — `lunaris:{scope}:episode:`. Chunk
// rows are never matched, so a forget never touches them. Recall stays clean
// only because `hydrate` has a second pass that looks up each chunk's parent
// episode and drops the hit when that episode is sys-closed:
//
//     .filter(|(_, chunk)| !matches!(episode_sources.get(&chunk.episode_id),
//                                    Some((_, true))))
//
// A soft forget stamps `bt.sys.1`, the lookup yields `Some((_, true))`, the
// chunk is dropped. A HARD forget issues `WriteOp::KvDelete` on the episode
// row, so the lookup yields **`None`** — and `!matches!(None, Some((_, true)))`
// is `true`. The chunk is kept and hydrates with an empty `source`.
//
// So the stronger operation is the leakier one: the irreversible, D-21
// confirmation-token-gated path that reports `rows_deleted` is the one that
// fails to hide the data, while the reversible default succeeds. Two locally
// reasonable decisions compose into it — forget's "v0 walks the episode kind"
// and hydrate's "missing episodes stay tolerated (empty source)" — and each
// one documents itself honestly in a comment.
//
// This is a data-deletion contract, not a ranking nicety: a caller who asks to
// hard-delete a conversation gets a receipt saying it happened, and the text
// still comes back from recall.
// ---------------------------------------------------------------------------

/// The bytes must be gone from recall after a confirmed hard delete.
#[tokio::test]
async fn hard_forget_removes_the_content_from_recall_moon() {
    let engine = open_moon().await;
    let scope = fresh_scope("hard-id");
    let scoped = engine.scoped(scope.clone());

    let ep_id = Ulid::new();
    scoped
        .ingest(
            EpisodeBuilder::new("wipe-me/hard", "the scarlet pendulum swings every 23 seconds")
                .id(ep_id),
        )
        .await
        .expect("ingest");

    let pre = scoped.recall(Query::text("scarlet pendulum swing interval")).await.expect("recall");
    assert!(recalls_episode(&pre, ep_id), "pre-forget recall must surface the episode: {pre:?}");

    // The D-21 two-step rail: preview, mint a token from that receipt, delete.
    let dry = scoped.forget(ForgetTarget::Id(ep_id).dry_run()).await.expect("dry-run forget");
    let token = engine.confirm_hard_forget(dry).await.expect("confirm");
    let receipt =
        scoped.forget(ForgetTarget::Id(ep_id).hard().with_token(token)).await.expect("hard forget");
    assert!(
        receipt.rows_deleted > 1,
        "a hard forget deleted {} row(s) — the episode alone, leaving its chunks \
         behind in KV and in the FT index",
        receipt.rows_deleted
    );

    let post = scoped.recall(Query::text("scarlet pendulum swing interval")).await.expect("recall");
    assert!(
        !recalls_episode(&post, ep_id),
        "a confirmed HARD delete reported rows_deleted={} and the content is STILL \
         recallable. The episode row is gone, so hydrate's parent-episode gate \
         yields None rather than Some((_, true)) and stops dropping the chunk. \
         Forget must reach the chunk rows, not rely on a gate that only fires \
         while the episode row survives. Hits: {post:?}",
        receipt.rows_deleted
    );
}

/// And the chunk text itself must not be returned under any provenance.
///
/// Separate from the assertion above on purpose: `recalls_episode` matches on
/// `Hit::episode_id`, and a hard-deleted parent leaves the hit's `source`
/// EMPTY — so a fix that merely blanked provenance could satisfy the sibling
/// test while the verbatim text still came back. This asserts the thing a
/// caller actually asked to be rid of.
#[tokio::test]
async fn hard_forget_leaves_no_recallable_text_moon() {
    let engine = open_moon().await;
    let scope = fresh_scope("hard-text");
    let scoped = engine.scoped(scope.clone());

    let ep_id = Ulid::new();
    let secret = "the scarlet pendulum swings every 23 seconds";
    scoped
        .ingest(EpisodeBuilder::new("wipe-me/hard-text", secret).id(ep_id))
        .await
        .expect("ingest");

    let pre = scoped.recall(Query::text("scarlet pendulum swing interval")).await.expect("recall");
    assert!(
        pre.iter().any(|h| h.text.contains("scarlet pendulum")),
        "pre-forget recall must return the text, or this test asserts nothing: {pre:?}"
    );

    let dry = scoped.forget(ForgetTarget::Id(ep_id).dry_run()).await.expect("dry-run forget");
    let token = engine.confirm_hard_forget(dry).await.expect("confirm");
    scoped.forget(ForgetTarget::Id(ep_id).hard().with_token(token)).await.expect("hard forget");

    let post = scoped.recall(Query::text("scarlet pendulum swing interval")).await.expect("recall");
    let leaked: Vec<&str> = post
        .iter()
        .filter(|h| h.text.contains("scarlet pendulum"))
        .map(|h| h.text.as_str())
        .collect();
    assert!(leaked.is_empty(), "the verbatim text survived a confirmed hard delete: {leaked:?}");
}

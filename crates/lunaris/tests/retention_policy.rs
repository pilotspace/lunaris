//! W4.6 / D6.4 — per-scope retention, against a live Moon.
//!
//! The D6 decision named two interactions to settle here rather than
//! rediscover, and both are asserted below:
//!
//! 1. **Soft-delete semantics.** `forget` soft-deletes by default; retention
//!    that hard-deletes must not silently change what `.hard()` means. A sweep
//!    is an ordinary scoped `ForgetTarget::Before`, so a soft sweep leaves the
//!    row recoverable and a hard sweep still goes through the D-21
//!    confirmation rail.
//! 2. **The `matched` over-count on soft-deleted records.** The decision
//!    predicted retention enforcement would surface it. It does, and worse
//!    than as a cosmetic count: `scan_matches_scoped` filtered only on the
//!    target predicate with no check for rows already sys-closed, so a
//!    repeating sweep re-stamped every row it had ever swept, forever. That is
//!    an unbounded rewrite loop on any scope with a retention policy, not a
//!    reporting bug.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::{EpisodeBuilder, Lunaris};
use lunaris_core::retention::RetentionPolicy;
use lunaris_core::{Scope, StoragePort};

fn moon_url() -> Option<String> {
    match std::env::var("MOON_URL").ok().filter(|s| !s.trim().is_empty()) {
        Some(u) => Some(u),
        None => {
            lunaris_test_harness::strict_skip::note_unavailable(
                "MOON_URL unset — retention_policy needs a live Moon",
            );
            None
        }
    }
}

async fn fresh(url: &str, tag: &str) -> (Arc<Lunaris>, Scope) {
    let lunaris = Arc::new(Lunaris::open(url).await.expect("open"));
    let scope = Scope::new(format!("{tag}{}", ulid::Ulid::new())).expect("scope");
    (lunaris, scope)
}

/// Count this scope's live (not sys-closed) episode rows straight off KV, so
/// the assertion does not depend on recall ranking or on an embedder.
async fn live_episodes(storage: &Arc<dyn StoragePort>, scope: &Scope) -> usize {
    use futures::StreamExt;
    let prefix = lunaris_core::keyspace::episode_prefix(scope);
    let mut stream = storage.scan_range(scope, &prefix, None).await.expect("scan");
    let mut live = 0usize;
    while let Some(item) = stream.next().await {
        let (_k, v) = item.expect("scan item");
        let json: serde_json::Value = match serde_json::from_slice(&v) {
            Ok(j) => j,
            Err(_) => continue,
        };
        let closed = json
            .get("bt")
            .and_then(|bt| bt.get("sys"))
            .and_then(|s| s.as_array())
            .and_then(|a| a.get(1))
            .map(|v| !v.is_null())
            .unwrap_or(false);
        if !closed {
            live += 1;
        }
    }
    live
}

/// Total episode rows present at all, live or tombstoned. A hard sweep must
/// move this; a soft sweep must not.
async fn total_episode_rows(storage: &Arc<dyn StoragePort>, scope: &Scope) -> usize {
    use futures::StreamExt;
    let prefix = lunaris_core::keyspace::episode_prefix(scope);
    let mut stream = storage.scan_range(scope, &prefix, None).await.expect("scan");
    let mut n = 0usize;
    while let Some(item) = stream.next().await {
        item.expect("scan item");
        n += 1;
    }
    n
}

#[tokio::test]
async fn a_scope_with_no_policy_is_never_swept() {
    let Some(url) = moon_url() else { return };
    let (lunaris, scope) = fresh(&url, "w46rt").await;
    let scoped = lunaris.scoped(scope.clone());
    scoped
        .ingest(EpisodeBuilder::new("keep/a", "the cobalt beacon flashes"))
        .await
        .expect("ingest");

    assert_eq!(scoped.retention_policy().await.expect("read policy"), None);

    let receipt = scoped.enforce_retention().await.expect("enforce must succeed with no policy");
    assert_eq!(receipt.policy, None, "a scope with no policy reported one");
    assert_eq!(receipt.rows_swept(), 0);
    assert!(
        receipt.forget.is_none(),
        "a scope with no policy ran a forget anyway — retention is opt-in, and the failure mode \
         of an accidental sweep is unrecoverable data loss"
    );
    assert_eq!(live_episodes(&lunaris.storage(), &scope).await, 1);
}

#[tokio::test]
async fn a_policy_round_trips_through_storage() {
    let Some(url) = moon_url() else { return };
    let (lunaris, scope) = fresh(&url, "w46rp").await;
    let scoped = lunaris.scoped(scope.clone());

    let policy = RetentionPolicy::max_age_ms(86_400_000);
    scoped.set_retention_policy(policy).await.expect("write policy");
    assert_eq!(scoped.retention_policy().await.expect("read policy"), Some(policy));

    // And it is per-scope: a sibling scope does not inherit it.
    let other = Scope::new(format!("w46rq{}", ulid::Ulid::new())).expect("scope");
    assert_eq!(lunaris.scoped(other).retention_policy().await.expect("read"), None);
}

#[tokio::test]
async fn the_age_bound_is_actually_applied() {
    let Some(url) = moon_url() else { return };
    let (lunaris, scope) = fresh(&url, "w46ra").await;
    let scoped = lunaris.scoped(scope.clone());
    scoped.ingest(EpisodeBuilder::new("keep/a", "the violet antenna hums")).await.expect("ingest");

    // A retention window wider than the age of anything in the store. The
    // cutoff lands in the past, so nothing is eligible. Without this the
    // suite could not tell "retention works" apart from "retention deletes
    // everything it is pointed at".
    scoped.set_retention_policy(RetentionPolicy::max_age_ms(u64::MAX)).await.expect("policy");
    let receipt = scoped.enforce_retention().await.expect("enforce");
    assert_eq!(receipt.rows_swept(), 0, "a window wider than the data's age still swept it");
    assert_eq!(live_episodes(&lunaris.storage(), &scope).await, 1);

    // Now a window of zero: everything written before now is eligible.
    scoped.set_retention_policy(RetentionPolicy::max_age_ms(0)).await.expect("policy");
    let receipt = scoped.enforce_retention().await.expect("enforce");
    assert!(receipt.rows_swept() > 0, "a zero-length window swept nothing");
    assert_eq!(live_episodes(&lunaris.storage(), &scope).await, 0);
}

#[tokio::test]
async fn a_soft_sweep_leaves_the_row_and_a_second_pass_is_a_no_op() {
    let Some(url) = moon_url() else { return };
    let (lunaris, scope) = fresh(&url, "w46rs").await;
    let scoped = lunaris.scoped(scope.clone());
    let storage = lunaris.storage();
    for i in 0..3 {
        scoped
            .ingest(EpisodeBuilder::new(format!("sweep/{i}"), format!("record number {i}")))
            .await
            .expect("ingest");
    }
    let rows_before = total_episode_rows(&storage, &scope).await;

    scoped.set_retention_policy(RetentionPolicy::max_age_ms(0)).await.expect("policy");

    let first = scoped.enforce_retention().await.expect("first sweep");
    let f = first.forget.as_ref().expect("a policied scope must run a forget");
    assert_eq!(
        f.rows_deleted, 0,
        "a SOFT policy hard-deleted; `.hard()` must keep meaning `.hard()`"
    );
    assert!(f.rows_written > 0, "the soft sweep stamped nothing");
    assert_eq!(live_episodes(&storage, &scope).await, 0, "soft-swept rows are still live");
    assert_eq!(
        total_episode_rows(&storage, &scope).await,
        rows_before,
        "a SOFT sweep removed rows — soft-delete must leave them recoverable"
    );

    // The interaction the D6 decision predicted. A second pass over an
    // already-swept scope must find nothing left to do. Before the scan
    // learned to skip sys-closed rows this re-stamped every row again, every
    // pass, forever — an unbounded rewrite loop on any scope with a policy.
    let second = scoped.enforce_retention().await.expect("second sweep");
    let s = second.forget.as_ref().expect("forget");
    assert_eq!(
        (s.matched, s.rows_written, s.rows_deleted),
        (0, 0, 0),
        "a second retention pass re-swept rows the first pass had already tombstoned: \
         matched={}, rows_written={}, rows_deleted={}",
        s.matched,
        s.rows_written,
        s.rows_deleted
    );
}

#[tokio::test]
async fn a_hard_sweep_removes_the_rows_and_still_goes_through_the_confirmation_rail() {
    let Some(url) = moon_url() else { return };
    let (lunaris, scope) = fresh(&url, "w46rh").await;
    let scoped = lunaris.scoped(scope.clone());
    let storage = lunaris.storage();
    scoped.ingest(EpisodeBuilder::new("burn/a", "the amber relay clicks")).await.expect("ingest");
    assert!(total_episode_rows(&storage, &scope).await > 0);

    scoped.set_retention_policy(RetentionPolicy::max_age_ms(0).hard()).await.expect("policy");

    let receipt = scoped.enforce_retention().await.expect(
        "a hard sweep must succeed. It derives its D-21 confirmation token from its own preview \
         receipt, exactly as a human hard-forget does — the policy is the standing \
         authorization, not a bypass of the rail. An error here means the sweep tried to hard \
         delete without one.",
    );
    let f = receipt.forget.as_ref().expect("forget");
    assert!(f.rows_deleted > 0, "a HARD policy soft-stamped instead of deleting");
    assert_eq!(total_episode_rows(&storage, &scope).await, 0, "a hard sweep left rows behind");
}

#[tokio::test]
async fn a_sweep_is_audited_on_the_scope_that_owns_the_data() {
    let Some(url) = moon_url() else { return };
    let (lunaris, scope) = fresh(&url, "w46rd").await;
    let scoped = lunaris.scoped(scope.clone());
    scoped.ingest(EpisodeBuilder::new("audit/a", "the ochre dial turns")).await.expect("ingest");
    scoped.set_retention_policy(RetentionPolicy::max_age_ms(0)).await.expect("policy");
    scoped.enforce_retention().await.expect("sweep");

    // Retention deletes data on a policy rather than on a request, which makes
    // it exactly the kind of action an audit trail exists for: nobody typed
    // the command, so the receipt is the only record it happened.
    let page = scoped.audit_events(None, None, 100).await.expect("read the trail");
    assert!(
        page.records.iter().any(|r| matches!(r.event, lunaris_core::audit::AuditEvent::Forget(_))),
        "a retention sweep left no audit receipt on the scope whose data it deleted; got {} \
         record(s)",
        page.records.len()
    );
}

#[tokio::test]
async fn an_unparseable_policy_is_an_error_not_a_silent_keep_everything() {
    let Some(url) = moon_url() else { return };
    let (lunaris, scope) = fresh(&url, "w46rx").await;
    let storage = lunaris.storage();
    storage
        .atomic_write(
            &scope,
            &[lunaris_core::WriteOp::KvPut {
                key: lunaris_core::retention::retention_policy_key(&scope),
                value: br#"{"maxAgeMs": 5}"#.to_vec(),
            }],
        )
        .await
        .expect("write a malformed policy");

    let err = lunaris.scoped(scope).retention_policy().await.expect_err(
        "a policy that does not parse must surface, not read as `None`. `None` means \
         KEEP EVERYTHING, so a typo'd field would silently disable retention for the scope \
         and nothing would ever say so.",
    );
    assert!(
        err.to_string().contains("retention policy"),
        "the error must name what failed; got: {err}"
    );
}

//! ADD task activation-ledger — engine-level RED/GREEN suite (§4 test_plan,
//! `crates/lunaris/tests/activation_ledger_engine.rs` row). Exercises
//! `ScopedLunaris::record_activation_refs` (the write half) and
//! `lunaris_retrieve::LedgerBoostProvider` (the read half) against a
//! hand-written in-process `StoragePort` double — same fixture style as
//! `phase_14_2_reflect_boost.rs`. The double is deliberate, not a leftover
//! from the deleted `memory://` backend: the subject is the ledger's exact
//! read/write arithmetic, which a real store's ranking would blur.

#![forbid(unsafe_code)]

mod common;

use lunaris_core::activation::{ActivationRecord, Grain, RefSignal, Strength};
use lunaris_core::keyspace::activation_key;
use lunaris_core::{HlcClock, Scope, StoragePort};
use lunaris_retrieve::{BoostProvider, LedgerBoostProvider, Query, Vector};
use std::sync::Arc;
use ulid::Ulid;

use common::{LedgerTestStorage, make_handle, seed_chunk, vector_hit};

// ---------------------------------------------------------------------------
// Test 1 — scenarios 1+2: upsert math through the engine writer, batched flush.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn record_refs_upserts_and_batches_one_atomic_write() {
    let scope = Scope::new("test.ledger-upsert").unwrap();
    let storage = Arc::new(LedgerTestStorage::default());
    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    let id_m = Ulid::new();

    // First signal: weak, turn-grain.
    scoped
        .record_activation_refs(&[RefSignal {
            id: id_m,
            grain: Grain::Turn,
            strength: Strength::Weak,
        }])
        .await
        .expect("first record_activation_refs must succeed");
    assert_eq!(storage.write_count(), 1, "first flush is one atomic_write");

    let key = activation_key(&scope, id_m);
    let row = storage.rows.lock().get(&key).cloned().expect("activation record must exist");
    let rec: ActivationRecord = serde_json::from_slice(&row.value).unwrap();
    assert_eq!(rec.n, 1);
    assert_eq!(rec.weighted, 1.0);
    assert_eq!(rec.last_grain, Grain::Turn);
    assert_eq!(rec.last_strength, Strength::Weak);
    assert_eq!(rec.first_ref_wall, rec.last_ref_wall, "both walls set identically on first signal");

    let first_wall = rec.first_ref_wall;

    // Second signal for the SAME id: strong, tool_call-grain — updates in place.
    scoped
        .record_activation_refs(&[RefSignal {
            id: id_m,
            grain: Grain::ToolCall,
            strength: Strength::Strong,
        }])
        .await
        .expect("second record_activation_refs must succeed");
    assert_eq!(storage.write_count(), 2, "second flush is a second atomic_write");

    let row2 = storage.rows.lock().get(&key).cloned().expect("activation record must still exist");
    let rec2: ActivationRecord = serde_json::from_slice(&row2.value).unwrap();
    assert_eq!(rec2.n, 2, "n increments");
    assert_eq!(rec2.weighted, 4.0, "weighted = weak(1.0) + strong(3.0)");
    assert_eq!(rec2.first_ref_wall, first_wall, "first_ref_wall unchanged");
    assert!(
        rec2.last_ref_wall >= first_wall,
        "last_ref_wall advances (or ties under a fast clock)"
    );
    assert_eq!(rec2.last_grain, Grain::ToolCall);
    assert_eq!(rec2.last_strength, Strength::Strong);

    // Batch of THREE distinct ids in ONE call flushes as exactly ONE atomic_write.
    let id_x = Ulid::new();
    let id_y = Ulid::new();
    let id_z = Ulid::new();
    scoped
        .record_activation_refs(&[
            RefSignal { id: id_x, grain: Grain::Turn, strength: Strength::Weak },
            RefSignal { id: id_y, grain: Grain::Turn, strength: Strength::Weak },
            RefSignal { id: id_z, grain: Grain::Turn, strength: Strength::Weak },
        ])
        .await
        .expect("batch record_activation_refs must succeed");
    assert_eq!(storage.write_count(), 3, "the 3-id batch is ONE additional atomic_write");
    assert_eq!(storage.last_batch().len(), 3, "the batch atomic_write carries exactly 3 KvPut ops");
}

// ---------------------------------------------------------------------------
// Test 2 — exit criterion: reinforced memory outranks an equal-similarity
// peer across a FRESH handle (no in-process cache carryover), real recall path.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reinforced_memory_outranks_across_handles() {
    let scope = Scope::new("test.ledger-cross-handle").unwrap();
    let clock = HlcClock::new(0);
    let id_a = Ulid::new();
    let id_b = Ulid::new();
    // Episode-grain ledger contract (PR #78): the boost pass keys priors by
    // the hit's PARENT EPISODE, and production ledger rows are written on
    // episode ids (memory.feedback resolves chunk→episode first). Each chunk
    // gets its own parent so a prior on A's episode can flip the tie.
    let ep_a = Ulid::new();
    let ep_b = Ulid::new();

    // B ranks first pre-boost (equal scores, B returned first) so any flip
    // to A is explained ONLY by the persisted activation ledger.
    let storage =
        Arc::new(LedgerTestStorage::new(vec![vector_hit(id_b, 0.80), vector_hit(id_a, 0.80)]));
    seed_chunk(&storage, &scope, id_a, ep_a, "chunk A", &clock);
    seed_chunk(&storage, &scope, id_b, ep_b, "chunk B", &clock);

    // Session 1: a fresh handle records two strong refs for A's parent
    // episode (the id the service layer would resolve to), then is dropped.
    {
        let handle1 = make_handle(storage.clone());
        let scoped1 = handle1.scoped(scope.clone());
        scoped1
            .record_activation_refs(&[
                RefSignal { id: ep_a, grain: Grain::ToolCall, strength: Strength::Strong },
                RefSignal { id: ep_a, grain: Grain::ToolCall, strength: Strength::Strong },
            ])
            .await
            .expect("session 1 record_activation_refs must succeed");
    } // handle1 dropped — its boost_cache is gone with it.

    // Session 2: a BRAND NEW handle over the SAME persisted storage. Its
    // boost_cache starts empty — any boost must come from the ledger, not an
    // in-process cache.
    let handle2 = make_handle(storage.clone());
    let scoped2 = handle2.scoped(scope.clone());

    let provider: Arc<dyn BoostProvider> =
        Arc::new(LedgerBoostProvider::new(storage.clone() as Arc<dyn StoragePort>));
    let hits = scoped2
        .dsl()
        .with_root(Vector::new("chunks", 30))
        .with_boost_provider(provider)
        .execute(Query::text("q"))
        .await
        .expect("recall with ledger provider must succeed");

    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].text, "chunk A",
        "A's persisted activation must outrank equal-similarity B: {hits:#?}"
    );

    // W1.8 — CONTRACT CORRECTION, not a weakened check. This block used to
    // assert that a default `dsl()` builder ignores the ledger ("without the
    // provider, pre-boost order must hold"). That was the frozen contract that
    // made `record_activation_refs` a public write half with no reachable read
    // half, so it is corrected rather than routed around. `dsl()` now carries
    // the provider on every surface, so the SAME ledger row that flips the tie
    // above must flip it here too — with nothing wired by hand.
    let hits_default =
        scoped2.dsl().with_root(Vector::new("chunks", 30)).execute(Query::text("q")).await.unwrap();
    assert_eq!(
        hits_default[0].text, "chunk A",
        "the default builder must read the same ledger the explicit provider does"
    );

    // The `LUNARIS_ACTIVATION_BOOST=0` opt-out is asserted in its own test
    // BINARY (`tests/activation_boost_optout.rs`) — env mutation is
    // process-global and the other nine tests in this file run in parallel
    // without an env guard, so toggling it here would race them.
}

// ---------------------------------------------------------------------------
// W1.8 — the SDK ships the WRITE half of the ledger and never reads it.
// ---------------------------------------------------------------------------

/// `ScopedLunaris::record_activation_refs` is public SDK surface: a caller can
/// write the activation ledger. `ScopedLunaris::dsl()` / `recall()` never wire
/// a `BoostProvider`, so those writes have NO effect on SDK recall — the write
/// half is reachable and the read half is not. The test above papers over this
/// by constructing a `LedgerBoostProvider` by hand, which no SDK caller can
/// even do: neither `BoostProvider` nor `LedgerBoostProvider` is re-exported
/// from the `lunaris` umbrella.
///
/// This is the same defect shape as the ledger itself being inert (F43): both
/// halves have tests, neither test crosses the production path.
///
/// Uses ONLY public SDK surface — no manual `with_boost_provider`.
#[tokio::test]
async fn sdk_recall_honors_the_ledger_the_sdk_lets_callers_write() {
    let scope = Scope::new("test.ledger-sdk-default").unwrap();
    let clock = HlcClock::new(0);
    let (id_a, id_b) = (Ulid::new(), Ulid::new());
    let (ep_a, ep_b) = (Ulid::new(), Ulid::new());

    // B ranks first pre-boost, so a flip to A is explained only by the ledger.
    let storage =
        Arc::new(LedgerTestStorage::new(vec![vector_hit(id_b, 0.80), vector_hit(id_a, 0.80)]));
    seed_chunk(&storage, &scope, id_a, ep_a, "chunk A", &clock);
    seed_chunk(&storage, &scope, id_b, ep_b, "chunk B", &clock);

    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    // WRITE half — public SDK.
    scoped
        .record_activation_refs(&[
            RefSignal { id: ep_a, grain: Grain::ToolCall, strength: Strength::Strong },
            RefSignal { id: ep_a, grain: Grain::ToolCall, strength: Strength::Strong },
        ])
        .await
        .expect("record_activation_refs must succeed");

    // READ half — public SDK, default builder, NOTHING wired by hand.
    let hits = scoped
        .dsl()
        .with_root(Vector::new("chunks", 30))
        .execute(Query::text("q"))
        .await
        .expect("recall must succeed");

    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].text, "chunk A",
        "the SDK let the caller WRITE this reinforcement; its own recall must \
         read it back. A write half with no read half is not a contract, it is \
         a no-op with a public name: {hits:#?}"
    );
}

/// Anti-vacuity companion. If the assertion above ever passes because every
/// hit is boosted (or because the fixture stopped discriminating), this fails:
/// a scope whose ledger was NEVER written must keep pre-boost order exactly.
#[tokio::test]
async fn sdk_recall_without_any_ledger_writes_keeps_preboost_order() {
    let scope = Scope::new("test.ledger-sdk-none").unwrap();
    let clock = HlcClock::new(0);
    let (id_a, id_b) = (Ulid::new(), Ulid::new());
    let (ep_a, ep_b) = (Ulid::new(), Ulid::new());

    let storage =
        Arc::new(LedgerTestStorage::new(vec![vector_hit(id_b, 0.80), vector_hit(id_a, 0.80)]));
    seed_chunk(&storage, &scope, id_a, ep_a, "chunk A", &clock);
    seed_chunk(&storage, &scope, id_b, ep_b, "chunk B", &clock);

    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    // No record_activation_refs at all.
    let hits = scoped
        .dsl()
        .with_root(Vector::new("chunks", 30))
        .execute(Query::text("q"))
        .await
        .expect("recall must succeed");

    assert_eq!(
        hits[0].text, "chunk B",
        "with an empty ledger the prior must be exactly zero, so pre-boost \
         order stands: {hits:#?}"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — reject: a corrupt activation record never fails recall.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn corrupt_record_recall_still_ok() {
    let scope = Scope::new("test.ledger-corrupt").unwrap();
    let clock = HlcClock::new(0);
    let id_a = Ulid::new();
    let ep = Ulid::new();

    let storage = Arc::new(LedgerTestStorage::new(vec![vector_hit(id_a, 0.55)]));
    seed_chunk(&storage, &scope, id_a, ep, "chunk A", &clock);
    // Garbage payload at A's activation key.
    storage.seed(activation_key(&scope, id_a), b"{not json".to_vec());

    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());
    let provider: Arc<dyn BoostProvider> =
        Arc::new(LedgerBoostProvider::new(storage.clone() as Arc<dyn StoragePort>));

    let hits = scoped
        .dsl()
        .with_root(Vector::new("chunks", 30))
        .with_boost_provider(provider)
        .execute(Query::text("q"))
        .await
        .expect("recall must return Ok even with a corrupt ledger row");

    assert_eq!(hits.len(), 1);
    assert!(
        (hits[0].score - 0.55).abs() < 1e-6,
        "corrupt record must not change the hit's score; got {}",
        hits[0].score
    );
}

// ---------------------------------------------------------------------------
// Test 4 — reject: a ledger write failure surfaces as Err (the CALLER, e.g.
// lunaris-hook::trace_injection, is the one that must log-and-continue).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ledger_write_failure_does_not_error_turn_path() {
    let scope = Scope::new("test.ledger-write-fail").unwrap();
    let storage = Arc::new(LedgerTestStorage::default());
    storage.fail_writes.store(true, std::sync::atomic::Ordering::SeqCst);

    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    let result = scoped
        .record_activation_refs(&[RefSignal {
            id: Ulid::new(),
            grain: Grain::Turn,
            strength: Strength::Weak,
        }])
        .await;
    assert!(
        result.is_err(),
        "record_activation_refs must surface the storage error to its caller — callers on the \
         turn path (trace_injection) are responsible for log-and-continue, matching the \
         apply_reflect_invalidate/apply_reflect_boost best-effort contract"
    );
}

// ---------------------------------------------------------------------------
// engram-soul-loop task 8b (`memory.distill`) — ScopedLunaris::archive_activation.
// ---------------------------------------------------------------------------

/// §4 test_plan: "archive_activation: marks existing records, skips missing
/// ids, returns correct count."
#[tokio::test]
async fn archive_activation_marks_existing_skips_missing_returns_count() {
    let scope = Scope::new("test.archive-activation-marks").unwrap();
    let storage = Arc::new(LedgerTestStorage::default());
    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    let id_a = Ulid::new();
    let id_b = Ulid::new();
    let id_missing = Ulid::new(); // never referenced — no ledger record exists

    scoped
        .record_activation_refs(&[
            RefSignal { id: id_a, grain: Grain::Turn, strength: Strength::Weak },
            RefSignal { id: id_b, grain: Grain::Turn, strength: Strength::Weak },
        ])
        .await
        .expect("seed refs for a and b");

    let now = 1_800_000_000u64;
    let archived_count = scoped
        .archive_activation(&[id_a, id_b, id_missing], now)
        .await
        .expect("archive_activation must succeed");
    assert_eq!(archived_count, 2, "only the two EXISTING records are counted; id_missing skipped");

    let rec_a: ActivationRecord = serde_json::from_slice(
        &storage.rows.lock().get(&activation_key(&scope, id_a)).unwrap().value,
    )
    .unwrap();
    assert_eq!(rec_a.archived_at, Some(now));
    assert!(rec_a.is_archived());

    let rec_b: ActivationRecord = serde_json::from_slice(
        &storage.rows.lock().get(&activation_key(&scope, id_b)).unwrap().value,
    )
    .unwrap();
    assert_eq!(rec_b.archived_at, Some(now));
    assert!(rec_b.is_archived());

    assert!(
        storage.rows.lock().get(&activation_key(&scope, id_missing)).is_none(),
        "archive_activation must NEVER create a record for an id with no prior reference \
         (already unboosted — nothing to mark)"
    );
}

/// Empty `ids` is a no-op — no storage call at all (mirrors
/// `record_activation_refs`'s empty-signals short-circuit).
#[tokio::test]
async fn archive_activation_empty_ids_is_noop() {
    let scope = Scope::new("test.archive-activation-empty").unwrap();
    let storage = Arc::new(LedgerTestStorage::default());
    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    let before = storage.write_count();
    let n = scoped.archive_activation(&[], 1_000).await.expect("empty ids must succeed");
    assert_eq!(n, 0);
    assert_eq!(storage.write_count(), before, "empty ids must not call atomic_write at all");
}

/// Every id missing a ledger record writes NOTHING (the batch has zero
/// KvPut ops, so `atomic_write` is never called) and returns `0`.
#[tokio::test]
async fn archive_activation_all_missing_ids_writes_nothing() {
    let scope = Scope::new("test.archive-activation-all-missing").unwrap();
    let storage = Arc::new(LedgerTestStorage::default());
    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    let before = storage.write_count();
    let n = scoped
        .archive_activation(&[Ulid::new(), Ulid::new()], 1_000)
        .await
        .expect("archive_activation over missing ids must still succeed");
    assert_eq!(n, 0);
    assert_eq!(storage.write_count(), before, "no atomic_write when nothing exists to mark");
}

/// §2 scenario: "archived sources lose their recall boost but stay
/// recallable." Real `LedgerBoostProvider` + real recall pipeline: archiving
/// A must (a) drop A's boost to 0 (B's equal-similarity tie is no longer
/// flipped toward A) and (b) leave A fully recallable (activation drop, not
/// a tombstone — vector_search still returns it, hydrate still resolves it).
#[tokio::test]
async fn archived_source_gets_zero_boost_but_stays_recallable() {
    let scope = Scope::new("test.archive-activation-boost-suppression").unwrap();
    let clock = HlcClock::new(0);
    let id_a = Ulid::new();
    let id_b = Ulid::new();
    // Episode-grain ledger contract (PR #78): ledger rows live on parent
    // episode ids and the boost pass keys each hit by its parent episode —
    // so each chunk needs its own parent for per-hit archive semantics.
    let ep_a = Ulid::new();
    let ep_b = Ulid::new();

    // Equal-similarity tie, A returned first pre-boost — any flip away from
    // A is explained only by B's surviving boost once A is archived.
    let storage =
        Arc::new(LedgerTestStorage::new(vec![vector_hit(id_a, 0.80), vector_hit(id_b, 0.80)]));
    seed_chunk(&storage, &scope, id_a, ep_a, "chunk A", &clock);
    seed_chunk(&storage, &scope, id_b, ep_b, "chunk B", &clock);

    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    scoped
        .record_activation_refs(&[
            RefSignal { id: ep_a, grain: Grain::ToolCall, strength: Strength::Strong },
            RefSignal { id: ep_a, grain: Grain::ToolCall, strength: Strength::Strong },
            RefSignal { id: ep_b, grain: Grain::ToolCall, strength: Strength::Strong },
            RefSignal { id: ep_b, grain: Grain::ToolCall, strength: Strength::Strong },
        ])
        .await
        .expect("seed refs for both a and b (episode-grain rows)");

    let now = 2_000_000_000u64;
    let archived = scoped.archive_activation(&[ep_a], now).await.expect("archive a's episode");
    assert_eq!(archived, 1);

    let provider: Arc<dyn BoostProvider> =
        Arc::new(LedgerBoostProvider::new(storage.clone() as Arc<dyn StoragePort>));

    // Direct priors() call — the frozen §4 test_plan explicitly allows this
    // form ("StubEmbedder recall or direct priors() call") — proven against
    // the REAL provider, not a stub of it.
    let priors = provider.priors(&scope, &[ep_a, ep_b]).await;
    assert!(
        !priors.contains_key(&ep_a),
        "archived A (episode row) must contribute 0 boost (omitted entirely): {priors:?}"
    );
    assert!(
        priors.get(&ep_b).copied().unwrap_or(0.0) > 0.0,
        "live B (episode row) must still boost: {priors:?}"
    );

    // Recall-level proof: A stays fully recallable (activation drop, not a
    // tombstone) and the tie now flips to B (only B's boost applies).
    let hits = scoped
        .dsl()
        .with_root(Vector::new("chunks", 30))
        .with_boost_provider(provider)
        .execute(Query::text("q"))
        .await
        .expect("recall must succeed with an archived source in the hit set");
    assert_eq!(hits.len(), 2, "archived source A must stay recallable: {hits:#?}");
    assert_eq!(
        hits[0].text, "chunk B",
        "only B's surviving boost applies now that A is archived — the tie flips to B: {hits:#?}"
    );
}

// ---------------------------------------------------------------------------
// W1.8 cost guard — the ledger read now lands on EVERY surface, so its cost
// per recall has to be pinned, not assumed.
// ---------------------------------------------------------------------------

/// `LedgerBoostProvider::priors` documents that its storage cost "MUST be
/// bounded by the ids slice — one batched pass whose storage cost is
/// proportional to the HIT SET, never to the scope's total". Before W1.8 that
/// MUST was unchecked prose and only the memory-service paid it. Now every
/// caller of `dsl()` does, including HTTP and all three SDKs, so it gets a
/// test: exactly one ledger read per DISTINCT hit, and none at all when the
/// caller opts out.
///
/// This is the guard that turns "the cost is fine" from an assumption into a
/// measurement. A regression that made priors scan the scope, or issue a read
/// per leg rather than per hit, would show up here as a count, not as a
/// latency wobble nobody can attribute.
#[tokio::test]
async fn ledger_read_cost_is_one_point_read_per_distinct_hit() {
    let scope = Scope::new("test.ledger-read-cost").unwrap();
    let clock = HlcClock::new(0);

    // Three hits, three distinct parent episodes, EMPTY ledger — the shape a
    // caller who never writes reinforcement signals actually has.
    let ids: Vec<(Ulid, Ulid)> = (0..3).map(|_| (Ulid::new(), Ulid::new())).collect();
    let storage =
        Arc::new(LedgerTestStorage::new(ids.iter().map(|(id, _)| vector_hit(*id, 0.80)).collect()));
    for (i, (id, ep)) in ids.iter().enumerate() {
        seed_chunk(&storage, &scope, *id, *ep, &format!("chunk {i}"), &clock);
    }

    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    storage.reads.lock().clear();
    let hits = scoped
        .dsl()
        .with_root(Vector::new("chunks", 30))
        .execute(Query::text("q"))
        .await
        .expect("recall must succeed against an empty ledger");
    assert_eq!(hits.len(), 3);

    let ledger_reads = storage.ledger_reads(&scope);
    assert_eq!(
        ledger_reads.len(),
        3,
        "one ledger point read per distinct hit — no more (a per-leg or \
         per-scope read pattern is a latency regression on every surface) and \
         no fewer (fewer means some hits silently skip their prior). Reads: \
         {ledger_reads:?}"
    );
    let distinct: std::collections::HashSet<_> = ledger_reads.iter().collect();
    assert_eq!(distinct.len(), ledger_reads.len(), "no key may be read twice");
}

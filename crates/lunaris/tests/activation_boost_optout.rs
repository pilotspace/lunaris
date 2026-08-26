//! W1.8 — `LUNARIS_ACTIVATION_BOOST=0` is the documented rollback lever for the
//! activation prior, so it gets a test that actually exercises it.
//!
//! This lives in its own test BINARY on purpose. `std::env::set_var` mutates a
//! process-global, and the nine tests in `activation_ledger_engine.rs` run in
//! parallel without an env guard — toggling the var there would race them and
//! turn a real regression into a flake (or, worse, hide one). One test, one
//! binary, no guard needed.
//!
//! The assertion is the exact inverse of
//! `sdk_recall_honors_the_ledger_the_sdk_lets_callers_write`: same fixture,
//! same ledger write, same default `dsl()` builder — only the env var differs.
//! That makes the pair discriminating in both directions: if the wiring were
//! unconditional, this test fails; if the wiring were absent, its twin fails.

// `std::env::set_var` is `unsafe` in Rust 2024 (MSRV 1.94). Permitted at the
// test-binary level ONLY — the production crate keeps `#![forbid(unsafe_code)]`
// and this binary holds a single test, so there is no concurrent reader.
#![allow(unsafe_code)]

mod common;

use std::sync::Arc;

use lunaris_core::activation::{Grain, RefSignal, Strength};
use lunaris_core::{HlcClock, Scope};
use lunaris_retrieve::{Query, Vector};
use ulid::Ulid;

use common::{LedgerTestStorage, make_handle, seed_chunk, vector_hit};

#[tokio::test]
async fn activation_boost_opt_out_restores_preboost_order() {
    let scope = Scope::new("test.ledger-sdk-optout").unwrap();
    let clock = HlcClock::new(0);
    let (id_a, id_b) = (Ulid::new(), Ulid::new());
    let (ep_a, ep_b) = (Ulid::new(), Ulid::new());

    // Identical fixture to the twin test: B first pre-boost, A reinforced.
    let storage =
        Arc::new(LedgerTestStorage::new(vec![vector_hit(id_b, 0.80), vector_hit(id_a, 0.80)]));
    seed_chunk(&storage, &scope, id_a, ep_a, "chunk A", &clock);
    seed_chunk(&storage, &scope, id_b, ep_b, "chunk B", &clock);

    let handle = make_handle(storage.clone());
    let scoped = handle.scoped(scope.clone());

    scoped
        .record_activation_refs(&[
            RefSignal { id: ep_a, grain: Grain::ToolCall, strength: Strength::Strong },
            RefSignal { id: ep_a, grain: Grain::ToolCall, strength: Strength::Strong },
        ])
        .await
        .expect("record_activation_refs must succeed even with the boost opted out");

    // SAFETY: single-test binary; no other thread reads the environment.
    unsafe { std::env::set_var("LUNARIS_ACTIVATION_BOOST", "0") };
    let hits = scoped
        .dsl()
        .with_root(Vector::new("chunks", 30))
        .execute(Query::text("q"))
        .await
        .expect("recall must succeed with the boost opted out");
    unsafe { std::env::remove_var("LUNARIS_ACTIVATION_BOOST") };

    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].text, "chunk B",
        "LUNARIS_ACTIVATION_BOOST=0 must leave pre-boost order untouched — this \
         is the documented rollback lever, and a lever that does not move is \
         worse than no lever: {hits:#?}"
    );
}

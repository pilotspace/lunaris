//! The destination's FT index width is decided by whoever opens the handle.
//!
//! Opening a Moon handle CREATES `chunks` / `entities` / `facts` /
//! `communities` when they are absent, and `FT.CREATE`'s `DIM` is sticky —
//! Moon will not resize. So a migration run silently fixes the destination's
//! vector width for every future ingest, even though it writes no vectors
//! itself. This file pins that the requested width is the width that lands.
//!
//! Needs a real Moon: the assertion is Moon's own sticky-dim guard. Under
//! `LUNARIS_TEST_BACKEND=memory` the test SKIPS loudly rather than passing
//! vacuously.

use lunaris_migrate::open::open_dest;
use lunaris_storage_moon::MoonStorage;
use lunaris_test_harness::{Backend, open_test_store};

const NON_DEFAULT_DIM: usize = 1536;

#[tokio::test]
async fn destination_indices_are_created_at_the_requested_dim() {
    let store = open_test_store().await;
    if store.backend() != Backend::Moon {
        eprintln!("skip destination_indices_are_created_at_the_requested_dim: needs a real Moon");
        return;
    }

    let _dest = open_dest(store.url(), NON_DEFAULT_DIM).await.expect("open destination at 1536-d");

    // Moon refuses a handle whose configured width disagrees with an existing
    // index. If `open_dest` had ignored `dim`, the indices would be 768-d and
    // this connect would succeed.
    let err = MoonStorage::connect_with_dim(store.url(), 768)
        .await
        .err()
        .expect("768-d handle must be refused once the indices exist at 1536-d");
    let msg = err.to_string();
    assert!(
        msg.contains("dim mismatch") && msg.contains("1536"),
        "expected Moon's sticky-dim refusal naming the existing width, got: {msg}"
    );
}

#[tokio::test]
async fn the_default_width_round_trips() {
    let store = open_test_store().await;
    if store.backend() != Backend::Moon {
        eprintln!("skip the_default_width_round_trips: needs a real Moon");
        return;
    }
    let _dest = open_dest(store.url(), 768).await.expect("open destination at 768-d");
    MoonStorage::connect_with_dim(store.url(), 768).await.expect("a matching handle reopens");
}

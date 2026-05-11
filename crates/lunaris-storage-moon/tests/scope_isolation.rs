//! Cross-scope leak integration test — RFC 0001 Wave 1C mandatory gate.
//!
//! Proves that Moon per-scope keyspace isolation works end-to-end:
//!
//! 1. Connect to Moon at `LUNARIS_MOON_URL` (default `moon://127.0.0.1:6380`).
//! 2. Write an episode under `scope_a` via `atomic_write(&scope_a, &ops)`.
//! 3. `vector_search(&scope_b, ...)` MUST return zero hits (no cross-scope bleed).
//! 4. `vector_search(&scope_a, ...)` MUST return the inserted row.
//!
//! ## How scope isolation is enforced in Moon
//!
//! Every Moon key is prefixed `lunaris:{scope}:` (for KV/hash entries) or
//! indexed under a per-scope FT index `lunaris_{scope}_{kind}_idx` (for vector
//! and keyword search). Because each scope has a disjoint keyspace prefix and
//! a separate FT index, a `FT.SEARCH lunaris_{scope_b}_{kind}_idx ...` query
//! can never return entries written under `scope_a`'s FT index — they live in
//! a completely different index name.
//!
//! This is structurally different from Postgres RLS (which filters rows in a
//! shared table). The Moon implementation prevents cross-scope reads by
//! construction: there is no shared index to query across.
//!
//! Gated behind the `moon-it` Cargo feature — requires a live Moon instance.
//!
//! ```bash
//! LUNARIS_MOON_URL=moon://127.0.0.1:6380 \
//!   cargo test -p lunaris-storage-moon --features moon-it --test scope_isolation
//! ```

#![cfg(feature = "moon-it")]

use bytes::Bytes;
use lunaris_core::{Episode, HlcClock, Scope, StoragePort, WriteOp};
use lunaris_storage_moon::MoonStorage;
use ulid::Ulid;

/// Returns the Moon URL from the environment variable, or the default.
fn moon_url() -> String {
    std::env::var("LUNARIS_MOON_URL").unwrap_or_else(|_| "moon://127.0.0.1:6380".into())
}

/// Cross-scope vector search returns zero hits for the wrong scope — RFC 0001 §3.6 Wave 1C.
///
/// Inserts an episode under `scope_a` and verifies that:
/// - `vector_search` under `scope_b` returns zero hits (no bleed).
/// - `vector_search` under `scope_a` returns the inserted row.
#[tokio::test]
#[ignore = "requires live Moon, run with LUNARIS_MOON_URL=moon://127.0.0.1:6380"]
async fn cross_scope_vector_search_returns_zero_for_wrong_scope() {
    let url = moon_url();

    let storage = match MoonStorage::connect(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("LUNARIS_MOON_URL not reachable ({e}); SKIP scope_isolation test");
            return;
        }
    };

    let scope_a = Scope::new("test-scope-a").unwrap();
    let scope_b = Scope::new("test-scope-b").unwrap();

    let clock = HlcClock::new();
    let ts = clock.now();

    // Create a unique episode ID to avoid collisions with other test runs.
    let ep_id = Ulid::new();
    let embedding: Vec<f32> = (0..384).map(|i| (i as f32) / 384.0).collect();

    let episode = Episode {
        id: ep_id,
        scope: scope_a.clone(),
        content: "scope isolation test episode".to_string(),
        source: "test".to_string(),
        t_ref: ts,
        bt: Default::default(),
        metadata: Default::default(),
    };

    // Write episode under scope_a.
    let ops = vec![WriteOp::UpsertEpisode {
        episode: episode.clone(),
        embedding: embedding.clone(),
    }];

    storage
        .atomic_write(&scope_a, &ops)
        .await
        .expect("atomic_write under scope_a must succeed");

    // vector_search under scope_b MUST return zero hits.
    let hits_b = storage
        .vector_search(&scope_b, &embedding, 10)
        .await
        .expect("vector_search under scope_b must not error");

    assert!(
        hits_b.is_empty(),
        "scope_b vector_search MUST return zero hits; got {}: this is a cross-scope data leak! \
         (RFC 0001 §3.6 violation)",
        hits_b.len()
    );

    // vector_search under scope_a MUST return the inserted episode.
    let hits_a = storage
        .vector_search(&scope_a, &embedding, 10)
        .await
        .expect("vector_search under scope_a must not error");

    assert!(
        !hits_a.is_empty(),
        "scope_a vector_search MUST return the inserted episode; got zero hits — \
         data may have been lost or indexed under the wrong scope"
    );

    let found = hits_a.iter().any(|h| h.id == ep_id);
    assert!(
        found,
        "inserted episode {ep_id} MUST appear in scope_a vector_search results; \
         got hits: {:?}",
        hits_a.iter().map(|h| h.id).collect::<Vec<_>>()
    );
}

/// Cross-scope KV bleed check — RFC 0001 §3.6 Wave 1C.
///
/// Writes a raw KV entry under scope_a's keyspace prefix and verifies
/// that the same key under scope_b's prefix does not exist. This catches
/// any regression where the keyspace prefix is omitted or miscomputed.
#[tokio::test]
#[ignore = "requires live Moon, run with LUNARIS_MOON_URL=moon://127.0.0.1:6380"]
async fn cross_scope_kv_keys_are_disjoint() {
    let url = moon_url();

    let storage = match MoonStorage::connect(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("LUNARIS_MOON_URL not reachable ({e}); SKIP scope_isolation test");
            return;
        }
    };

    let scope_a = Scope::new("kv-test-scope-a").unwrap();
    let scope_b = Scope::new("kv-test-scope-b").unwrap();

    let clock = HlcClock::new();
    let ts = clock.now();

    let ep_id = Ulid::new();
    let embedding: Vec<f32> = vec![0.1f32; 384];

    let episode = Episode {
        id: ep_id,
        scope: scope_a.clone(),
        content: "kv disjoint test".to_string(),
        source: "test".to_string(),
        t_ref: ts,
        bt: Default::default(),
        metadata: Default::default(),
    };

    let ops = vec![WriteOp::UpsertEpisode {
        episode,
        embedding,
    }];

    storage
        .atomic_write(&scope_a, &ops)
        .await
        .expect("write under scope_a must succeed");

    // KV read under scope_b for the same ULID must return None.
    // (read_as_of uses the scoped episode key internally.)
    let result_b = storage.read_as_of(&scope_b, ep_id, ts).await;
    match result_b {
        Ok(None) => {} // correct — no bleed
        Ok(Some(_)) => panic!(
            "scope_b read_as_of returned Some for episode {ep_id} written under scope_a — \
             cross-scope KV leak detected (RFC 0001 §3.6 violation)"
        ),
        Err(e) => {
            // A "not found" / "key does not exist" error is also acceptable.
            let s = e.to_string().to_lowercase();
            if s.contains("not found") || s.contains("no such") || s.contains("missing") {
                // Fine — key does not exist in scope_b's keyspace.
            } else {
                panic!("scope_b read_as_of errored unexpectedly: {e}");
            }
        }
    }

    // KV read under scope_a for the same ULID MUST return Some.
    let result_a = storage
        .read_as_of(&scope_a, ep_id, ts)
        .await
        .expect("read_as_of under scope_a must not error");

    assert!(
        result_a.is_some(),
        "scope_a read_as_of MUST return the written episode {ep_id}; \
         got None — data may be lost or stored under wrong key"
    );
}

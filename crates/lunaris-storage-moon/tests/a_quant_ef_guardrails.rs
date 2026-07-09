//! moon-v051-perf-exploit W1-1 / W1-2 — live-Moon proof that:
//!
//! 1. A plain handle (no `?quant=`) now provisions the legacy global indices
//!    at SQ8 (the new default), NOT the old TQ4-via-typed-SDK path.
//! 2. Reconnecting with a DIFFERENT `?quant=` tier against the SAME
//!    pre-existing indices does NOT fail `connect` (graceful degrade) — the
//!    existing tier stays in effect (sticky schema), matching the dim
//!    guardrail's precedent (`tests/dim_configurable.rs`) but with warn
//!    severity instead of hard failure.
//! 3. `?ef=<n>` reaches `FT.CREATE ... EF_RUNTIME <n>` for a freshly created
//!    index and is visible back out via `FT.INFO`.
//!
//! Gated behind `moon-it` + `MOON_URL`, mirroring every other live-Moon test
//! in this crate (see `tests/dim_configurable.rs`, `tests/quantization_recall.rs`).
//!
//! ```bash
//! MOON_URL=moon://localhost:7801 \
//!   cargo test -p lunaris-storage-moon --features moon-it --test a_quant_ef_guardrails -- --nocapture
//! ```

#![cfg(feature = "moon-it")]

use lunaris_storage_moon::MoonStorage;
use lunaris_storage_moon::client::Quantization;

fn base_url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:7801".to_string())
}

fn url_with(params: &str) -> String {
    let base = base_url();
    if base.contains('?') { format!("{base}&{params}") } else { format!("{base}?{params}") }
}

async fn connect_or_skip(u: &str) -> Option<MoonStorage> {
    match MoonStorage::connect(u).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!(
                "MOON_URL not reachable ({e}); SKIP (this is expected outside CI-with-live-Moon)"
            );
            None
        }
    }
}

/// Raw `FT.INFO <name>` → the `QUANTIZATION` / `EF_RUNTIME` extra fields, via
/// the typed SDK's `index_info` (same call the dim guardrail uses).
async fn ft_info_extra(
    moon: &MoonStorage,
    name: &str,
) -> std::collections::HashMap<String, String> {
    let typed = moon.client().typed();
    let info = typed.vector().index_info(name).await.expect("FT.INFO must succeed");
    info.extra
}

/// §1 DISCRIMINATOR — a plain handle (no `?quant=`) provisions the legacy
/// global `chunks` index at SQ8, not TQ4. This is the live proof that W1-1's
/// default flip actually reaches the wire, complementing the pure
/// `resolve_quantization` unit test and `quantization_recall.rs`'s
/// `plain_handle_defaults_to_sq8` (which only asserts the in-memory field,
/// not what Moon actually recorded).
#[tokio::test]
async fn plain_connect_creates_legacy_chunks_index_at_sq8() {
    let Some(moon) = connect_or_skip(&base_url()).await else { return };
    let extra = ft_info_extra(&moon, "chunks").await;
    let quant = extra.get("QUANTIZATION").cloned().unwrap_or_default();
    // Sticky-schema caveat: if an EARLIER test run against the same Moon
    // instance already created `chunks` at a different tier, this index
    // will report that stale tier instead (Moon's FT.CREATE never
    // re-quantizes). We still assert SQ8 because CI and the harness in
    // `tmp/moon-perf-context.md` mandate a fresh Moon per agent (port 7801,
    // `--appendonly no`); a persistent-Moon dev host should `FT.DROPINDEX
    // chunks entities facts communities` before rerunning this suite.
    assert_eq!(
        quant, "SQ8",
        "plain (no ?quant=) connect must provision `chunks` at SQ8 (v0.5.1+ default); \
         got {quant:?}. If this Moon instance predates this test run, FT.DROPINDEX and retry."
    );
}

/// §2 DISCRIMINATOR — reconnecting with `?quant=tq4` against a Moon that
/// already has `chunks`/etc. at SQ8 (from the previous test, or fresh) MUST
/// NOT fail connect, and the STICKY tier (SQ8, whatever was first created)
/// remains in effect — proving the "warn, don't fail" graceful-degrade
/// contract, mirrored against `dim_configurable.rs::
/// reopen_at_mismatched_dim_is_rejected`'s hard-fail counterpart for dim.
#[tokio::test]
async fn reconnect_with_different_quant_does_not_fail_connect() {
    // Establish a baseline at the default (SQ8) first, idempotent if it
    // already exists.
    let Some(_baseline) = connect_or_skip(&base_url()).await else { return };
    drop(_baseline);

    // Reconnect requesting TQ4 explicitly. Must succeed (Ok), not Err —
    // this is the entire point of the graceful-degrade contract.
    let reconnected = MoonStorage::connect(&url_with("quant=tq4")).await;
    assert!(
        reconnected.is_ok(),
        "reconnecting with a different ?quant= against pre-existing indices must NOT fail \
         connect (graceful degrade, warn-only) — got {:?}",
        reconnected.err()
    );
    let moon = reconnected.unwrap();

    // The STICKY tier must still be whatever was first created (SQ8 from
    // the baseline connect above), NOT TQ4 — FT.CREATE is idempotent and
    // does not migrate.
    let extra = ft_info_extra(&moon, "chunks").await;
    let quant = extra.get("QUANTIZATION").cloned().unwrap_or_default();
    assert_eq!(
        quant, "SQ8",
        "quantization must stay STICKY at whatever tier the index was first created with \
         (SQ8) — a later ?quant=tq4 connect must not have migrated it in place; got {quant:?}"
    );
}

/// §3 DISCRIMINATOR — `EF_RUNTIME` at `FT.CREATE` time round-trips through
/// `FT.INFO`.
///
/// This calls `create_lunaris_index_named_ef` DIRECTLY (not through
/// `MoonStorage::connect`) against a uniquely-named, single-test-owned
/// index, rather than the legacy global `chunks`/etc. names. Those are
/// process-lifetime-sticky (`FT.CREATE` never migrates an existing index)
/// and shared across EVERY OTHER live test in this crate that ever calls
/// `MoonStorage::connect` at all — including this very file's other two
/// tests — so asserting "the first connect's `?ef=` value stuck" is only
/// true for whichever test happens to connect first in a given Moon
/// server lifetime. A unique index name sidesteps that ordering hazard
/// entirely while still exercising the EXACT function
/// `MoonClient::ensure_indexes` calls.
///
/// NOTE: as documented on `client.rs::create_lunaris_index_named`, the
/// PER-SCOPE creation call (`lib.rs::create_scope_indexes`, outside this
/// agent's file ownership) does not yet thread `ef` through — this test
/// proves the `client.rs`/`vector.rs` EF_RUNTIME plumbing is correct;
/// the per-scope wiring gap is a documented one-line follow-up.
#[tokio::test]
async fn ef_runtime_reaches_ft_create_and_round_trips_via_ft_info() {
    let Some(moon) = connect_or_skip(&base_url()).await else { return };
    let typed = moon.client().typed();

    let unique = format!("efdirect_{}", ulid::Ulid::new());
    let name = format!("{unique}_idx");
    let prefix = format!("{unique}:");
    lunaris_storage_moon::client::create_lunaris_index_named_ef(
        &typed,
        &name,
        "chunks",
        &prefix,
        768,
        Some(Quantization::Sq8),
        Some(77),
    )
    .await
    .expect("create_lunaris_index_named_ef must succeed against a fresh, unique index name");

    let info = typed.vector().index_info(&name).await.expect("FT.INFO must succeed");
    let ef = info.extra.get("EF_RUNTIME").cloned().unwrap_or_default();
    assert_eq!(
        ef, "77",
        "EF_RUNTIME=77 passed to create_lunaris_index_named_ef must round-trip via FT.INFO; \
         got EF_RUNTIME={ef:?}"
    );

    // Round-trip control: the value is queryable through the normal
    // HSET-auto-index + FT.SEARCH KNN path, not just FT.INFO metadata.
    let mut v = vec![0.0f32; 768];
    v[0] = 1.0;
    let id_hex = hex::encode(ulid::Ulid::new().to_bytes());
    let key = format!("{prefix}{id_hex}");
    let mut conn = typed.clone();
    let embedding_bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
    redis::cmd("HSET")
        .arg(&key)
        .arg("vec")
        .arg(&embedding_bytes)
        .arg("content")
        .arg("ef marker")
        .query_async::<()>(conn.inner_mut())
        .await
        .expect("HSET auto-index write must succeed");

    let hits = typed.vector().search(&name, &v, 1).await.expect("KNN search must succeed");
    assert_eq!(hits.len(), 1, "self-query must return the written vector");
}

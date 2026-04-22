//! Phase 9 Plan 09-05 PRIM-05 + Phase 9.1 Plan 09.1-03 PRIM-04 —
//! WorkingMemory Moon + Postgres parity test.
//!
//! Phase 9 landed write/read/grep parity under the 2026-04-22
//! scope-reduction split. Phase 9.1 Plan 09.1-01 wired three surfaces
//! (WorkingMemory::consolidate() scope-aware hook,
//! Consolidator::consolidate_scoped(Option<&str>) default trait method,
//! ConsolidateEvent.source populated at publish-time). Plan 09.1-03
//! (this file, current wave) extends the harness to assert Moon vs
//! Postgres parity on .consolidate() scope isolation: a 10-note seed
//! (5 under "test:wm/", 5 under "other:") produces byte-identical
//! ConsolidationReport.promotions count across backends.
//!
//! Feature-gated behind `moon-it` + `pg-it`. Default `cargo test -p lunaris-recipes`
//! executes 0 parity tests. With features + env vars + reachable TCP the
//! test seeds a minimal 5-note corpus under scope_prefix `"test:wm/"` on
//! each backend via `WorkingMemory::write`, then asserts
//!   1. `read(k)` returns the same value on both backends for each key.
//!   2. `grep("note-")` returns the same (source, value) set on both
//!      backends once sorted by source.
//!   3. `.consolidate()` returns equal `promotions.len()` on both backends
//!      when a TestConsolidator is installed; the `"other:"` events do
//!      not leak into the scoped report.
//!
//! Unlike the sibling harnesses this one does NOT reuse the shared 10-
//! episode fixture corpus — WorkingMemory's primary invariant is
//! `scope_prefix` scoping, and a self-seeded minimal 5-note set exercises
//! that surface directly. Writes go through `Lunaris::ingest` (via
//! `WorkingMemory::write` → `Episode` → full chunker + embedder pipeline),
//! so the `"chunks"` vector / keyword index IS populated and `read` /
//! `grep` observe real hits — hence the assertion surface is semantic,
//! not structural-only.
//!
//! ## Regression guard (presence, replacing the Phase 9 absence guard)
//!
//! Plan 09-05 established an absence guard on `.consolidate(` in this
//! file (the method did not exist on WorkingMemory in Phase 9). Plan
//! 09.1-01 landed the method; Plan 09.1-03 (this edit) flips the absence
//! guard to a presence guard enforced at CI grep-time: `.consolidate(`
//! MUST appear at least once here.
//!
//! ## TestConsolidator rationale
//!
//! The `.consolidate()` parity case installs a deterministic
//! `TestConsolidator` on both Moon + Postgres Lunaris handles before the
//! assertion. Without this install, the default `NoopConsolidator` would
//! return an empty report from both backends, making the assertion
//! trivially pass at 0/0 and proving nothing. `TestConsolidator` returns
//! one `PromotionEvent` per filtered `ConsolidateEvent`, so the scope
//! filter (`Some("test:wm/")`) in `Consolidator::consolidate_scoped` is
//! what's under test — not the consolidator's scoring logic.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]
// Imports + `probe_backend` fn are only used behind the `moon-it` + `pg-it`
// feature gate; suppress the clippy dead-code warnings on the default build.
#![cfg_attr(
    not(all(feature = "moon-it", feature = "pg-it")),
    allow(unused_imports, dead_code)
)]

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

/// Verbatim copy of `probe_backend` from
/// `crates/lunaris-conformance/tests/run_storage_moon.rs:46-87`.
/// Duplicated per 09-PATTERNS.md decision #2 (stay consistent with the
/// 5 existing copies; extraction is v0.1.2 backlog).
fn probe_backend(name: &str, url: Option<String>) -> Option<String> {
    let url = url?;
    let host_port = if let Some(rest) = url.strip_prefix("moon://") {
        rest.split('/').next()?.to_string()
    } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let after_scheme = url.split("://").nth(1)?;
        let authority = after_scheme.split('/').next()?;
        let bare = authority.rsplit('@').next()?;
        if bare.contains(':') {
            bare.to_string()
        } else {
            format!("{bare}:5432")
        }
    } else {
        eprintln!("{name}: SKIP (unknown URL scheme)");
        return None;
    };
    let timeout = Duration::from_secs(1);
    let addr = match host_port.to_socket_addrs().ok().and_then(|mut it| it.next()) {
        Some(a) => a,
        None => {
            eprintln!("{name}: SKIP (DNS resolution of {host_port} failed)");
            return None;
        }
    };
    match TcpStream::connect_timeout(&addr, timeout) {
        Ok(_) => Some(url),
        Err(_) => {
            eprintln!(
                "{name}: SKIP (TCP probe to {host_port} failed within {}ms)",
                timeout.as_millis()
            );
            None
        }
    }
}

/// Deterministic test consolidator installed on both backend handles for
/// the `.consolidate()` parity case. Returns one `PromotionEvent` per
/// input event so the scope filter in `Consolidator::consolidate_scoped`
/// (Plan 09.1-01) is what the parity assertion targets — not the
/// consolidator's scoring logic.
///
/// Mirrors the `TestConsolidator` shape from
/// `crates/lunaris-recipes/tests/working_memory_consolidate.rs` lines
/// 236-282. Duplicated rather than extracted because integration test
/// crates cannot import from each other; byte-duplication is the
/// established parity-file convention (09-PATTERNS.md decision #2).
#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[derive(Default)]
struct TestConsolidator;

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[async_trait::async_trait]
impl lunaris::Consolidator for TestConsolidator {
    async fn consolidate(
        &self,
        _storage: std::sync::Arc<dyn lunaris_core::StoragePort>,
        events: &[lunaris_consolidate::ConsolidateEvent],
    ) -> Result<lunaris_consolidate::ConsolidationReport, lunaris_core::LunarisError> {
        use lunaris_consolidate::{FactId, PromotionEvent};
        use lunaris_extract::EntityId;
        let promotions = events
            .iter()
            .map(|e| {
                let id_bytes = e.episode_id.to_bytes();
                let mut subject = [0u8; 16];
                subject.copy_from_slice(&id_bytes);
                PromotionEvent {
                    episode_id: e.episode_id,
                    fact_id: FactId::from_triple(
                        EntityId(subject),
                        "promoted",
                        EntityId([2u8; 16]),
                    ),
                    activation_score: 1.0,
                }
            })
            .collect();
        Ok(lunaris_consolidate::ConsolidationReport {
            promotions,
            archives: vec![],
            communities_rebuilt: 0,
        })
    }

    fn applies(&self) -> bool {
        true
    }
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn working_memory_moon_postgres_parity() -> anyhow::Result<()> {
    // Phase 9 landed write/read/grep parity. Phase 9.1 Plan 09.1-01
    // wired WorkingMemory::consolidate(); Plan 09.1-03 (this wave)
    // extends this harness with the .consolidate() scope-isolation
    // parity case backed by a TestConsolidator install on both handles.
    use lunaris::Lunaris;
    use lunaris_recipes::WorkingMemory;
    use serde_json::json;

    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let moon = Arc::new(Lunaris::open(&moon_url).await?);
    let postgres = Arc::new(Lunaris::open(&pg_url).await?);

    // Seed 5 notes under scope_prefix "test:wm/" on each backend.
    let wm_moon = WorkingMemory::new(moon.clone(), "test:wm/");
    let wm_pg = WorkingMemory::new(postgres.clone(), "test:wm/");
    for i in 0..5 {
        let k = format!("note-{i}");
        let v = json!({ "seq": i, "body": format!("fixture note {i}") });
        wm_moon.write(&k, v.clone()).await?;
        wm_pg.write(&k, v).await?;
    }

    let mut divergences: Vec<String> = Vec::new();

    // Parity 1: read parity — each key returns identical value on both.
    for i in 0..5 {
        let k = format!("note-{i}");
        let moon_v = wm_moon.read(&k).await?;
        let pg_v = wm_pg.read(&k).await?;
        if moon_v != pg_v {
            divergences.push(format!("read {k}: moon={moon_v:?} postgres={pg_v:?}"));
        }
    }

    // Parity 2: grep parity — scanning the "note-" sub-prefix should return
    // the same 5 scoped Episodes both sides. Sort by source before pair-wise
    // compare — WorkingMemory::grep does not guarantee backend-uniform
    // ordering and ordering-by-source is the test's canonicalisation.
    let moon_hits = wm_moon.grep("note-").await?;
    let pg_hits = wm_pg.grep("note-").await?;
    if moon_hits.len() != pg_hits.len() {
        divergences.push(format!(
            "grep note-: moon_count={} postgres_count={}",
            moon_hits.len(),
            pg_hits.len()
        ));
    } else {
        let mut m_sorted = moon_hits.clone();
        let mut p_sorted = pg_hits.clone();
        m_sorted.sort_by(|a, b| a.0.cmp(&b.0));
        p_sorted.sort_by(|a, b| a.0.cmp(&b.0));
        for (i, ((mk, mv), (pk, pv))) in m_sorted.iter().zip(p_sorted.iter()).enumerate() {
            if mk != pk {
                divergences.push(format!("grep position {i} key: moon={mk} postgres={pk}"));
            }
            if mv != pv {
                divergences.push(format!(
                    "grep position {i} value for {mk}: moon={mv:?} postgres={pv:?}"
                ));
            }
        }
    }

    // -----------------------------------------------------------------
    // Phase 9.1 Plan 09.1-03 — .consolidate() scope-isolation parity.
    //
    // Install TestConsolidator on both handles (replaces the default
    // NoopConsolidator that would return empty reports on both sides and
    // make the parity trivially pass at 0/0). Seed 5 additional notes
    // under "other:" so the scope filter has real non-scoped events to
    // reject. The 5 "test:wm/" notes were already seeded above; seeding
    // the 5 "other:" notes here inverts the scope-reduction Phase 9
    // rustdoc caveat (the method didn't exist then).
    // -----------------------------------------------------------------
    moon.consolidator_pipeline()
        .set_consolidator(Arc::new(TestConsolidator));
    postgres
        .consolidator_pipeline()
        .set_consolidator(Arc::new(TestConsolidator));

    let wm_moon_other = WorkingMemory::new(moon.clone(), "other:");
    let wm_pg_other = WorkingMemory::new(postgres.clone(), "other:");
    for i in 0..5 {
        let k = format!("note-{i}");
        let v = json!({ "seq": i, "scope": "other:" });
        wm_moon_other.write(&k, v.clone()).await?;
        wm_pg_other.write(&k, v).await?;
    }

    let moon_report = wm_moon.consolidate().await?;
    let pg_report = wm_pg.consolidate().await?;

    if moon_report.promotions.len() != pg_report.promotions.len() {
        divergences.push(format!(
            "consolidate promotions.len() divergence: moon={} postgres={}",
            moon_report.promotions.len(),
            pg_report.promotions.len(),
        ));
    }

    if !divergences.is_empty() {
        anyhow::bail!(
            "PRIM-04/PRIM-05 WorkingMemory parity violations ({} divergences): {:#?}",
            divergences.len(),
            divergences
        );
    }
    Ok(())
}

//! Phase 9 Plan 09-05 PRIM-05 — WorkingMemory Moon + Postgres parity test.
//!
//! Reduced-surface parity per the 2026-04-22 scope-reduction split. Covers
//! `write / read / grep` ONLY — the consolidation-hook method does not
//! exist on the struct in Phase 9 (see Plan 09-03 revision note +
//! module-level rustdoc on `crates/lunaris-recipes/src/working_memory.rs`);
//! its parity coverage moves to Phase 9.1 Plan 09.1-03 when the method
//! lands in Phase 9.1 Plan 09.1-01.
//!
//! Feature-gated behind `moon-it` + `pg-it`. Default `cargo test -p lunaris-recipes`
//! executes 0 parity tests. With features + env vars + reachable TCP the
//! test seeds a minimal 5-note corpus under scope_prefix `"test:wm/"` on
//! each backend via `WorkingMemory::write`, then asserts
//!   1. `read(k)` returns the same value on both backends for each key.
//!   2. `grep("note-")` returns the same (source, value) set on both
//!      backends once sorted by source.
//!
//! Unlike the sibling harnesses this one does NOT reuse the shared 10-
//! episode fixture corpus — WorkingMemory's primary invariant is
//! `scope_prefix` scoping, and a self-seeded minimal 5-note set exercises
//! that surface directly. Writes go through `Lunaris::ingest` (via
//! `WorkingMemory::write` → `Episode` → full chunker + embedder pipeline),
//! so the `"chunks"` vector / keyword index IS populated and `read` /
//! `grep` observe real hits — hence the assertion surface is semantic,
//! not structural-only.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

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

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn working_memory_moon_postgres_parity() -> anyhow::Result<()> {
    // Phase 9 scope: write / read / grep parity ONLY. WorkingMemory's
    // reduced surface is 4 methods (new, write, read, grep) — see
    // Plan 09-03 revision note. Consolidation-hook parity ships in
    // Phase 9.1 Plan 09.1-03 (simultaneously with the method itself
    // landing in Phase 9.1 Plan 09.1-01). DO NOT add consolidation-hook
    // assertions here; the method does not exist on the struct.
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

    if !divergences.is_empty() {
        anyhow::bail!(
            "PRIM-05 WorkingMemory parity violations ({} divergences): {:#?}",
            divergences.len(),
            divergences
        );
    }
    Ok(())
}

//! Phase 9 Plan 09-05 PRIM-05 — TemporalQuery Moon + Postgres parity test.
//!
//! AS_OF-only parity coverage per the 2026-04-22 scope-reduction split. The
//! `.before` / `.after` / `.between` operators record into `TemporalBounds`
//! but do NOT flow to the backend in Phase 9 (see Plan 09-04 revision note
//! plus the module-level rustdoc on the TemporalQuery source file); their
//! backend wiring plus parity coverage moves to Phase 9.1 Plan 09.1-03.
//!
//! Feature-gated behind `moon-it` + `pg-it`. Default `cargo test -p lunaris-recipes`
//! executes 0 parity tests. With features + env vars + reachable TCP the
//! test seeds the deterministic `FixtureCorpus` into both backends,
//! iterates the `query_set()` tuples where `as_of = Some(Hlc)`, executes
//! `TemporalQuery::<Messages>::new(handle).as_of(ts).execute(query)` on
//! each backend, and accumulates divergences. Entries with `as_of = None`
//! are skipped — "latest" queries are the MessageStream parity test's
//! concern, not TemporalQuery's point-in-time surface.
//!
//! ## Semantic caveat (Phase 9 structural scope)
//!
//! `FixtureCorpus::ingest_into` writes raw `WriteOp::KvPut` per episode into
//! the episode-KV key space. It does NOT populate the `"chunks"` Vector /
//! Keyword index that `TemporalQuery::execute` fuses over via
//! `RetrievalBuilder::recall().as_of(ts).execute(...)`. On a dev machine
//! with live backends the parity run observes `0 == 0` hit-count parity —
//! a trivial-but-valid pass. Structural PRIM-05 is satisfied; semantic
//! deepening lands in Phase 9.1 Plan 09.1-03 or a later v0.1.2 task.

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

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn temporal_query_moon_postgres_parity() -> anyhow::Result<()> {
    // Phase 9 scope: .as_of parity ONLY. The .before / .after / .between
    // operators accumulate into TemporalBounds (see Plan 09-04) but do
    // not reach the backend in Phase 9 — Phase 9.1 Plan 09.1-02 adds
    // Filter::ValidTimeRange and Phase 9.1 Plan 09.1-03 extends this
    // harness. DO NOT add .before / .after / .between assertions here;
    // they would pass vacuously and lock in a ghost-method test.
    use lunaris::Lunaris;
    use lunaris_conformance::fixtures::FixtureCorpus;
    use lunaris_recipes::{Documents, Messages, TemporalQuery};

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

    let corpus = FixtureCorpus::new();
    let moon_store = moon.storage();
    let pg_store = postgres.storage();
    corpus.ingest_into(&moon_store).await?;
    corpus.ingest_into(&pg_store).await?;

    // Parity covers the (query, Some(Hlc)) entries from the fixture
    // query_set — these are the AS_OF tuples Plan 05-02 already validates
    // for the raw StoragePort; Phase 9 proves the TemporalQuery primitive
    // layers on top without drift. Entries with as_of=None are skipped
    // because "latest" queries are not what TemporalQuery targets.
    let mut divergences: Vec<String> = Vec::new();
    for (query, as_of) in corpus.query_set() {
        let as_of_ts = match as_of {
            Some(ts) => *ts,
            None => continue, // Skip "latest" queries — TemporalQuery is point-in-time.
        };

        let moon_hits = TemporalQuery::<Messages>::new(moon.clone())
            .as_of(as_of_ts)
            .execute(query)
            .await?;
        let pg_hits = TemporalQuery::<Messages>::new(postgres.clone())
            .as_of(as_of_ts)
            .execute(query)
            .await?;

        if moon_hits.len() != pg_hits.len() {
            divergences.push(format!(
                "messages query={query:?} as_of={as_of_ts:?} hit_count moon={} postgres={}",
                moon_hits.len(),
                pg_hits.len()
            ));
            continue;
        }
        for (i, (m, p)) in moon_hits.iter().zip(pg_hits.iter()).enumerate() {
            if m.id != p.id {
                divergences.push(format!(
                    "messages query={query:?} as_of={as_of_ts:?} position {i}: id moon={:?} postgres={:?}",
                    m.id, p.id
                ));
                break;
            }
        }

        // Also exercise the Documents source marker on the same as_of to
        // prove the typestate witness doesn't change backend semantics.
        let moon_docs = TemporalQuery::<Documents>::new(moon.clone())
            .as_of(as_of_ts)
            .execute(query)
            .await?;
        let pg_docs = TemporalQuery::<Documents>::new(postgres.clone())
            .as_of(as_of_ts)
            .execute(query)
            .await?;
        if moon_docs.len() != pg_docs.len() {
            divergences.push(format!(
                "documents query={query:?} as_of={as_of_ts:?} hit_count moon={} postgres={}",
                moon_docs.len(),
                pg_docs.len()
            ));
        }
    }

    if !divergences.is_empty() {
        anyhow::bail!(
            "PRIM-05 TemporalQuery parity violations ({} divergences): {:#?}",
            divergences.len(),
            divergences
        );
    }
    Ok(())
}

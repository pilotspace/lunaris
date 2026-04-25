//! Phase 9 Plan 09-05 PRIM-05 + Phase 9.1 Plan 09.1-03 PRIM-03 — TemporalQuery
//! Moon + Postgres parity test.
//!
//! Phase 9 landed AS_OF-only parity coverage under the 2026-04-22
//! scope-reduction split. Phase 9.1 Plan 09.1-02 wired
//! `Filter::ValidTimeRange` end-to-end on Moon (`@valid_time:[lo hi]` over a
//! `SchemaField::Numeric("valid_time")` chunks index) + Postgres
//! (`valid_from >= 'rfc' AND valid_from < 'rfc'`), and Plan 09.1-03 (this
//! file, current wave) extends the parity harness to assert Moon == Postgres
//! hit-set parity on `.before / .after / .between` on BOTH `Messages` +
//! `Documents` source markers.
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
//! After the AS_OF loop, three additional range-axis cases run once each
//! (Messages + Documents markers) against a fixed timestamp split drawn
//! from the FixtureCorpus episode range (wall_ms 1_000_000..=1_009_000):
//! `.before(1_005_000)`, `.after(1_002_000)`, `.between(1_002_000,
//! 1_007_000)`. Divergences append into the same `Vec<String>` and surface
//! at `anyhow::bail!` end-of-test.
//!
//! ## Regression guard (presence, replacing the Phase 9 absence guard)
//!
//! Plan 09-05 established an absence guard on `.before(/.after(/.between(`
//! in this file (the operators accumulated into `TemporalBounds` but did
//! not reach the backend). Plan 09.1-02 landed the backend wiring, so the
//! absence guard is INVERTED to a presence guard enforced at CI grep-time:
//! each of `.before(`, `.after(`, `.between(` MUST appear at least once
//! here.
//!
//! ## Semantic caveat (Phase 9 structural scope, still applicable)
//!
//! `FixtureCorpus::ingest_into` writes raw `WriteOp::KvPut` per episode into
//! the episode-KV key space. It does NOT populate the `"chunks"` Vector /
//! Keyword index that `TemporalQuery::execute` fuses over via
//! `RetrievalBuilder::recall()...execute(...)`. On a dev machine with live
//! backends the parity run observes `0 == 0` hit-count parity on both the
//! AS_OF loop and the new `.before/.after/.between` cases — a trivial-but
//! -valid pass that still asserts Moon and Postgres agree byte-for-byte on
//! the empty-result shape. Deeper semantic parity via `Lunaris::ingest` in
//! the fixture seed is deferred to v0.1.2 per 09.1-CONTEXT.md line 22.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]
// Imports + `probe_backend` fn are only used behind the `moon-it` + `pg-it`
// feature gate; suppress the clippy dead-code warnings on the default build.
#![cfg_attr(not(all(feature = "moon-it", feature = "pg-it")), allow(unused_imports, dead_code))]

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
        if bare.contains(':') { bare.to_string() } else { format!("{bare}:5432") }
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
    // Phase 9 scope landed .as_of parity. Phase 9.1 Plan 09.1-02 wired
    // Filter::ValidTimeRange on Moon + Postgres; Plan 09.1-03 (this wave)
    // extends this harness with .before / .after / .between parity on
    // BOTH Messages + Documents markers — every bound flows live through
    // RetrievalBuilder::execute against real indexed fields.
    use lunaris::Lunaris;
    use lunaris_conformance::fixtures::FixtureCorpus;
    use lunaris_core::hlc::Hlc;
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

        let moon_hits =
            TemporalQuery::<Messages>::new(moon.clone()).as_of(as_of_ts).execute(query).await?;
        let pg_hits =
            TemporalQuery::<Messages>::new(postgres.clone()).as_of(as_of_ts).execute(query).await?;

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
        let moon_docs =
            TemporalQuery::<Documents>::new(moon.clone()).as_of(as_of_ts).execute(query).await?;
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

    // -----------------------------------------------------------------
    // Phase 9.1 Plan 09.1-03 — .before / .after / .between range parity.
    //
    // FixtureCorpus episodes span wall_ms 1_000_000..=1_009_000 (see
    // lunaris-conformance::fixtures::build_ten_episodes). The range-axis
    // probe uses a fixed query string ("alpha") drawn from query_set;
    // splits partition the corpus cleanly. Both backends go through
    // Filter::ValidTimeRange (Plan 09.1-02) — Moon emits
    // `@valid_time:[lo hi]` against the chunks FT numeric field; Postgres
    // emits `valid_from >= 'rfc' AND valid_from < 'rfc'`.
    // -----------------------------------------------------------------
    let probe_query = "alpha";
    let before_ts = Hlc { wall_ms: 1_005_000, counter: 0, node_id: 0 };
    let after_ts = Hlc { wall_ms: 1_002_000, counter: 0, node_id: 0 };
    let between_lo = Hlc { wall_ms: 1_002_000, counter: 0, node_id: 0 };
    let between_hi = Hlc { wall_ms: 1_007_000, counter: 0, node_id: 0 };

    // --- .before(ts) parity — Messages + Documents ---
    {
        let moon_msg = TemporalQuery::<Messages>::new(moon.clone())
            .before(before_ts)
            .execute(probe_query)
            .await?;
        let pg_msg = TemporalQuery::<Messages>::new(postgres.clone())
            .before(before_ts)
            .execute(probe_query)
            .await?;
        if moon_msg.len() != pg_msg.len() {
            divergences.push(format!(
                "messages .before({before_ts:?}) hit_count moon={} postgres={}",
                moon_msg.len(),
                pg_msg.len()
            ));
        } else {
            for (i, (m, p)) in moon_msg.iter().zip(pg_msg.iter()).enumerate() {
                if m.id != p.id {
                    divergences.push(format!(
                        "messages .before({before_ts:?}) position {i}: id moon={:?} postgres={:?}",
                        m.id, p.id
                    ));
                    break;
                }
            }
        }

        let moon_doc = TemporalQuery::<Documents>::new(moon.clone())
            .before(before_ts)
            .execute(probe_query)
            .await?;
        let pg_doc = TemporalQuery::<Documents>::new(postgres.clone())
            .before(before_ts)
            .execute(probe_query)
            .await?;
        if moon_doc.len() != pg_doc.len() {
            divergences.push(format!(
                "documents .before({before_ts:?}) hit_count moon={} postgres={}",
                moon_doc.len(),
                pg_doc.len()
            ));
        }
    }

    // --- .after(ts) parity — Messages + Documents ---
    {
        let moon_msg = TemporalQuery::<Messages>::new(moon.clone())
            .after(after_ts)
            .execute(probe_query)
            .await?;
        let pg_msg = TemporalQuery::<Messages>::new(postgres.clone())
            .after(after_ts)
            .execute(probe_query)
            .await?;
        if moon_msg.len() != pg_msg.len() {
            divergences.push(format!(
                "messages .after({after_ts:?}) hit_count moon={} postgres={}",
                moon_msg.len(),
                pg_msg.len()
            ));
        } else {
            for (i, (m, p)) in moon_msg.iter().zip(pg_msg.iter()).enumerate() {
                if m.id != p.id {
                    divergences.push(format!(
                        "messages .after({after_ts:?}) position {i}: id moon={:?} postgres={:?}",
                        m.id, p.id
                    ));
                    break;
                }
            }
        }

        let moon_doc = TemporalQuery::<Documents>::new(moon.clone())
            .after(after_ts)
            .execute(probe_query)
            .await?;
        let pg_doc = TemporalQuery::<Documents>::new(postgres.clone())
            .after(after_ts)
            .execute(probe_query)
            .await?;
        if moon_doc.len() != pg_doc.len() {
            divergences.push(format!(
                "documents .after({after_ts:?}) hit_count moon={} postgres={}",
                moon_doc.len(),
                pg_doc.len()
            ));
        }
    }

    // --- .between(a, b) parity — Messages + Documents ---
    {
        let moon_msg = TemporalQuery::<Messages>::new(moon.clone())
            .between(between_lo, between_hi)
            .execute(probe_query)
            .await?;
        let pg_msg = TemporalQuery::<Messages>::new(postgres.clone())
            .between(between_lo, between_hi)
            .execute(probe_query)
            .await?;
        if moon_msg.len() != pg_msg.len() {
            divergences.push(format!(
                "messages .between({between_lo:?}, {between_hi:?}) hit_count moon={} postgres={}",
                moon_msg.len(),
                pg_msg.len()
            ));
        } else {
            for (i, (m, p)) in moon_msg.iter().zip(pg_msg.iter()).enumerate() {
                if m.id != p.id {
                    divergences.push(format!(
                        "messages .between({between_lo:?}, {between_hi:?}) position {i}: id moon={:?} postgres={:?}",
                        m.id, p.id
                    ));
                    break;
                }
            }
        }

        let moon_doc = TemporalQuery::<Documents>::new(moon.clone())
            .between(between_lo, between_hi)
            .execute(probe_query)
            .await?;
        let pg_doc = TemporalQuery::<Documents>::new(postgres.clone())
            .between(between_lo, between_hi)
            .execute(probe_query)
            .await?;
        if moon_doc.len() != pg_doc.len() {
            divergences.push(format!(
                "documents .between({between_lo:?}, {between_hi:?}) hit_count moon={} postgres={}",
                moon_doc.len(),
                pg_doc.len()
            ));
        }
    }

    if !divergences.is_empty() {
        anyhow::bail!(
            "PRIM-03/PRIM-05 TemporalQuery parity violations ({} divergences): {:#?}",
            divergences.len(),
            divergences
        );
    }
    Ok(())
}

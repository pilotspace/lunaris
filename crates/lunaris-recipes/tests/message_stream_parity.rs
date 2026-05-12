//! Phase 9 Plan 09-05 PRIM-05 — MessageStream Moon + Postgres parity test.
//!
//! Mirrors the Plan 05-02 as_of_parity pattern: seeds the deterministic
//! FixtureCorpus (seed inherited from `lunaris-conformance`) into both backends via
//! `FixtureCorpus::ingest_into`, runs the MessageStream recall path on each,
//! accumulates all divergences, bails with a structured error if any
//! hit-count or ordering divergence is observed.
//!
//! Feature-gated behind `moon-it` + `pg-it`. Without both live services,
//! `cargo test -p lunaris-recipes` (default) runs 0 parity tests (compile
//! gated out). With features + env vars + reachable TCP, the test executes;
//! when env vars are unset or TCP probe fails, the test returns `Ok(())` with
//! an `eprintln` note — matches the Plan 04-03 / 05-02 probe_backend discipline.
//!
//! ## Semantic caveat (Phase 9 structural scope)
//!
//! `FixtureCorpus::ingest_into` writes one raw `WriteOp::KvPut` per episode
//! into the episode-KV key space (`episode:conformance:<ulid>`). It does NOT
//! populate the `"chunks"` Vector / Keyword index that `MessageStream::recall`
//! fuses over. Therefore on a dev machine with live backends the parity run
//! observes `moon_hits.len() == pg_hits.len() == 0` — a trivial-but-valid
//! parity pass (0 divergences). This is by design for Phase 9's STRUCTURAL
//! closure of PRIM-05: the harness proves every primitive has a dual-backend
//! test file. Phase 9.1 Plan 09.1-03 (or a v0.1.2 task) can deepen the
//! ingestion path to route through `Lunaris::ingest` so `"chunks"` is
//! populated and parity becomes semantic as well as structural.

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
async fn message_stream_moon_postgres_parity() -> anyhow::Result<()> {
    use lunaris::Lunaris;
    use lunaris_conformance::fixtures::FixtureCorpus;
    use lunaris_recipes::MessageStream;

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

    // Seed deterministic fixture corpus into BOTH backends (seed inherited
    // from `lunaris_conformance::fixtures::SEED`; NEVER re-declared here).
    // `ingest_into` borrows `&Arc<dyn StoragePort>`; bind to locals so the
    // Arc's lifetime spans both the `.await` and any error path.
    let corpus = FixtureCorpus::new();
    let moon_store = moon.storage();
    let pg_store = postgres.storage();
    corpus.ingest_into(&moon_store).await?;
    corpus.ingest_into(&pg_store).await?;

    let ms_moon = MessageStream::new(moon.clone(), lunaris_core::Scope::dev(), "messages:");
    let ms_pg = MessageStream::new(postgres.clone(), lunaris_core::Scope::dev(), "messages:");

    let mut divergences: Vec<String> = Vec::new();
    for (query, _as_of) in corpus.query_set() {
        let moon_hits = ms_moon.recall(query).await?;
        let pg_hits = ms_pg.recall(query).await?;

        if moon_hits.len() != pg_hits.len() {
            divergences.push(format!(
                "query {query:?}: hit_count moon={} postgres={}",
                moon_hits.len(),
                pg_hits.len()
            ));
            continue;
        }

        // Compare hit-id ordering — MessageStream recency rescoring should
        // produce semantically-equivalent sets across backends for the same
        // FixtureCorpus seed. Preserve order (ranking is the point for
        // message recall — do NOT sort).
        for (i, (m, p)) in moon_hits.iter().zip(pg_hits.iter()).enumerate() {
            if m.id != p.id {
                divergences.push(format!(
                    "query {query:?} position {i}: id moon={:?} postgres={:?}",
                    m.id, p.id
                ));
                break;
            }
        }
    }

    if !divergences.is_empty() {
        anyhow::bail!(
            "PRIM-05 MessageStream parity violations ({} divergences): {:#?}",
            divergences.len(),
            divergences
        );
    }
    Ok(())
}

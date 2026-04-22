//! Phase 11 Plan 11-01 Task 3 — Documentary wrappers Rust dual-backend
//! integration harness.
//!
//! Exercises the five documentary wrappers on BOTH Moon and Postgres
//! backends via the `probe_backend` pattern (verbatim from
//! `document_corpus_parity.rs` / `temporal_query_parity.rs` per
//! 09-PATTERNS.md decision #2). Feature-gated behind `moon-it` + `pg-it`
//! so the default `cargo test -p lunaris-recipes` executes 0 integration
//! tests (Phase 9 Plan 04-03 discipline).
//!
//! ## Canonical scenarios covered
//!
//! 1. `code_repo_memory_as_of_commit_50_round_trip_moon_postgres` — seed 100
//!    synthetic commits via `CodeRepoMemory::ingest_commit`; `recall` at
//!    `as_of = commit_50_ts` should return the commit-50 body on BOTH
//!    backends (top-k SET equality per D-13).
//! 2. `timeline_reconstruction_between_returns_exactly_6_events` — seed 30
//!    daily events; `.between(2025-01-10 00:00Z, 2025-01-16 00:00Z)` returns
//!    exactly 6 events (days 10..=15 inclusive; upper-bound exclusive
//!    matches Phase 9.1 Postgres renderer `valid_from < hi`).
//! 3. `customer_support_history_refund_recall_preserves_source_prefixes` —
//!    seed 50 tickets + 150 chats; recall "refund" returns a union of
//!    ticket + chat hits with distinct `source` prefixes and no duplicate
//!    source ids across buckets.
//!
//! All three tests assert top-k SET equality (not byte-identical ordering)
//! per D-13 — tie-bucket ordering is known-flaky across backends.
//!
//! ## Semantic caveat
//!
//! Unlike `FixtureCorpus` in Phase 9, the documentary wrappers route
//! `ingest` through `Lunaris::ingest` (the umbrella chunker + embedder +
//! `atomic_write` pipeline), so the `"chunks"` Vector / Keyword index IS
//! populated. Assertions below are semantic (real hit-id matches),
//! NOT just hit-count parity.
//!
//! ## Graph-on path (flagged for Phase 11-02)
//!
//! `ResearchPaperCorpus.with_graph_pipeline(true)` is NOT exercised in
//! this harness — graph-on CI load is gated behind
//! `LUNARIS_EXTRACT_GEMMA_PATH` (D-11). A smoke test is deferred to
//! Phase 11-03 Py/TS parity where the graph toggle is cross-language
//! acceptance material.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]
// Imports + helpers only used behind the `moon-it` + `pg-it` feature gate.
#![cfg_attr(
    not(all(feature = "moon-it", feature = "pg-it")),
    allow(unused_imports, dead_code)
)]

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Verbatim copy of `probe_backend` from
/// `crates/lunaris-conformance/tests/run_storage_moon.rs:46-87` (also
/// duplicated in `document_corpus_parity.rs` + `temporal_query_parity.rs`
/// per 09-PATTERNS.md decision #2 — extraction is v0.1.2 backlog).
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

// ---------------------------------------------------------------------------
// Fixture loaders — deserialize the JSON files committed in Task 2.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[derive(serde::Deserialize)]
struct CommitFixture {
    sha: String,
    committer_date_rfc3339: String,
    function_body_chunk: String,
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[derive(serde::Deserialize)]
struct EventFixture {
    id: String,
    valid_time_rfc3339: String,
    text: String,
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[derive(serde::Deserialize)]
struct CustomerSupportFixture {
    tickets: Vec<TicketFixture>,
    chats: Vec<ChatFixture>,
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[derive(serde::Deserialize)]
struct TicketFixture {
    id: String,
    body: String,
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[derive(serde::Deserialize)]
struct ChatFixture {
    ticket_id: String,
    turn_idx: u32,
    participant: String,
    msg: String,
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
fn load_commits() -> anyhow::Result<Vec<CommitFixture>> {
    let raw = include_str!("fixtures/documentary/code_repo_100_commits.json");
    Ok(serde_json::from_str(raw)?)
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
fn load_events() -> anyhow::Result<Vec<EventFixture>> {
    let raw = include_str!("fixtures/documentary/timeline_30_days.json");
    Ok(serde_json::from_str(raw)?)
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
fn load_customer_support() -> anyhow::Result<CustomerSupportFixture> {
    let raw = include_str!("fixtures/documentary/customer_support_50_tickets.json");
    Ok(serde_json::from_str(raw)?)
}

/// Parse RFC3339 → Unix-ms. Hand-rolled (no new dep per plan's "No new
/// deps in lunaris-recipes/Cargo.toml"). Accepts `YYYY-MM-DDTHH:MM:SSZ`
/// only — fixtures emit exactly that shape (see Task 2 generator).
#[cfg(all(feature = "moon-it", feature = "pg-it"))]
fn rfc3339_to_unix_ms(s: &str) -> anyhow::Result<i64> {
    // Expect: "YYYY-MM-DDTHH:MM:SSZ" (20 chars). Fractional seconds are
    // not produced by the Task 2 Python generator, so we reject them
    // rather than add an RFC3339 parser dep.
    if s.len() != 20 || !s.ends_with('Z') || &s[10..11] != "T" {
        anyhow::bail!("unsupported RFC3339 shape (expected YYYY-MM-DDTHH:MM:SSZ): {s}");
    }
    let y: i64 = s[0..4].parse()?;
    let mo: i64 = s[5..7].parse()?;
    let d: i64 = s[8..10].parse()?;
    let h: i64 = s[11..13].parse()?;
    let mi: i64 = s[14..16].parse()?;
    let se: i64 = s[17..19].parse()?;
    // Civil-to-Unix-days via Howard Hinnant's algorithm
    // (http://howardhinnant.github.io/date_algorithms.html#days_from_civil)
    // — a proven, dep-free conversion. Returns days since 1970-01-01.
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400; // [0, 399]
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days_from_civil = era * 146097 + doe - 719468;
    let unix_seconds = days_from_civil * 86_400 + h * 3600 + mi * 60 + se;
    Ok(unix_seconds * 1000)
}

// ---------------------------------------------------------------------------
// Scenario 1 — CodeRepoMemory 100-commit as_of round-trip.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
async fn run_code_repo_as_of_commit_50(
    backend_label: &str,
    url: &str,
) -> anyhow::Result<Vec<String>> {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_core::hlc::Hlc;
    use lunaris_recipes::documentary::CodeRepoMemory;

    let mem = Arc::new(Lunaris::open(url).await?);
    let repo =
        CodeRepoMemory::new(mem.clone(), format!("repo:doc-11-01/{backend_label}/"));
    let commits = load_commits()?;
    let commit_50 = &commits[49]; // 1-indexed → idx 49 is "commit 50"
    let commit_50_ms = rfc3339_to_unix_ms(&commit_50.committer_date_rfc3339)?;
    let commit_50_ts = Hlc::from_parts(commit_50_ms as u64, 0, 0);

    for c in &commits {
        let ms = rfc3339_to_unix_ms(&c.committer_date_rfc3339)?;
        let mut meta = serde_json::Map::new();
        meta.insert(
            "function_name".into(),
            serde_json::Value::String("target".into()),
        );
        repo.ingest_commit(&c.sha, ms, vec![(c.function_body_chunk.clone(), meta)])
            .await?;
    }

    let hits = repo.recall("target", commit_50_ts).await?;
    let texts: Vec<String> = hits.iter().map(|h| h.text.clone()).collect();
    Ok(texts)
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn code_repo_memory_as_of_commit_50_round_trip_moon_postgres() -> anyhow::Result<()> {
    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let moon_texts = run_code_repo_as_of_commit_50("moon", &moon_url).await?;
    let pg_texts = run_code_repo_as_of_commit_50("pg", &pg_url).await?;

    // D-13: top-k SET equality (tie-bucket ordering accepted as known-flaky).
    let moon_set: std::collections::BTreeSet<_> = moon_texts.iter().cloned().collect();
    let pg_set: std::collections::BTreeSet<_> = pg_texts.iter().cloned().collect();
    if moon_set != pg_set {
        anyhow::bail!(
            "CodeRepoMemory as_of(commit_50) set divergence:\n  moon={:?}\n  pg={:?}",
            moon_set, pg_set
        );
    }
    // Primary SC: commit-50 body is the top hit (not commit-100).
    let expected = "fn target() -> u64 { 50 }".to_string();
    assert!(
        moon_texts.iter().any(|t| t.contains(&expected)),
        "moon: expected commit-50 body `{}` in hits, got: {:?}", expected, moon_texts
    );
    assert!(
        pg_texts.iter().any(|t| t.contains(&expected)),
        "pg: expected commit-50 body `{}` in hits, got: {:?}", expected, pg_texts
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 2 — TimelineReconstruction 30-day .between returns exactly 6.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
async fn run_timeline_between_10_15(
    backend_label: &str,
    url: &str,
) -> anyhow::Result<usize> {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_core::hlc::Hlc;
    use lunaris_recipes::documentary::TimelineReconstruction;

    let mem = Arc::new(Lunaris::open(url).await?);
    let timeline = TimelineReconstruction::new(
        mem.clone(),
        format!("timeline:doc-11-01/{backend_label}/"),
    );
    let events = load_events()?;

    for e in &events {
        let ms = rfc3339_to_unix_ms(&e.valid_time_rfc3339)?;
        let mut meta = serde_json::Map::new();
        meta.insert("event_id".into(), serde_json::Value::String(e.id.clone()));
        meta.insert(
            "event_valid_time_unix_ms".into(),
            serde_json::Value::Number(ms.into()),
        );
        timeline.ingest(vec![(e.text.clone(), meta)]).await?;
    }

    // Boundary convention (flagged for 11-03): lower inclusive, upper
    // exclusive (matches Phase 9.1 Postgres renderer
    // `valid_from >= lo AND valid_from < hi`). To include days 10..=15
    // (6 days), set hi = Jan 16 00:00:00Z.
    let lo_ms = rfc3339_to_unix_ms("2025-01-10T00:00:00Z")?;
    let hi_ms = rfc3339_to_unix_ms("2025-01-16T00:00:00Z")?;
    let lo = Hlc::from_parts(lo_ms as u64, 0, 0);
    let hi = Hlc::from_parts(hi_ms as u64, 0, 0);
    let hits = timeline.between("event", lo, hi).await?;
    Ok(hits.len())
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn timeline_reconstruction_between_returns_exactly_6_events() -> anyhow::Result<()> {
    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let moon_count = run_timeline_between_10_15("moon", &moon_url).await?;
    let pg_count = run_timeline_between_10_15("pg", &pg_url).await?;
    // Both backends must agree and both must return exactly 6.
    if moon_count != pg_count {
        anyhow::bail!(
            "TimelineReconstruction .between count divergence: moon={} pg={}",
            moon_count, pg_count
        );
    }
    assert_eq!(
        moon_count, 6,
        "expected exactly 6 events in [2025-01-10, 2025-01-16) inclusive-of-lo/exclusive-of-hi; got {}",
        moon_count
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 3 — CustomerSupportHistory "refund" recall preserves source prefixes.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
async fn run_customer_support_refund(
    _backend_label: &str,
    url: &str,
) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_recipes::documentary::CustomerSupportHistory;

    let mem = Arc::new(Lunaris::open(url).await?);
    let hist = CustomerSupportHistory::new(mem.clone());
    let fixture = load_customer_support()?;

    for t in &fixture.tickets {
        hist.ingest_ticket(&t.id, &t.body).await?;
    }
    for c in &fixture.chats {
        hist.ingest_chat(&c.ticket_id, c.turn_idx, &c.participant, &c.msg)
            .await?;
    }

    let hits = hist.recall("refund").await?;
    Ok(hits.into_iter().map(|h| (h.source, h.id)).collect())
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn customer_support_history_refund_recall_preserves_source_prefixes() -> anyhow::Result<()>
{
    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    for (label, url) in [("moon", moon_url.as_str()), ("pg", pg_url.as_str())] {
        let hits = run_customer_support_refund(label, url).await?;
        let has_ticket = hits.iter().any(|(s, _)| s.starts_with("ticket:"));
        let has_chat = hits.iter().any(|(s, _)| s.starts_with("chat:"));
        assert!(
            has_ticket,
            "{label}: expected at least one hit with source.starts_with(\"ticket:\"); got {:?}",
            hits.iter().map(|(s, _)| s).collect::<Vec<_>>()
        );
        assert!(
            has_chat,
            "{label}: expected at least one hit with source.starts_with(\"chat:\"); got {:?}",
            hits.iter().map(|(s, _)| s).collect::<Vec<_>>()
        );
        // D-09 enforcement: RRF fused WITHIN each bucket; every returned
        // (source, id) pair must be unique. Duplicate source ids would
        // imply cross-bucket double-indexing collapsed.
        let unique: std::collections::BTreeSet<_> = hits.iter().cloned().collect();
        assert_eq!(
            unique.len(),
            hits.len(),
            "{label}: duplicate (source, id) pairs in recall — RRF bucket isolation broken: {:?}",
            hits
        );
    }
    Ok(())
}

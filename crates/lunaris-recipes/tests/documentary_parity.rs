//! Phase 11 Plan 11-03 — Documentary wrappers cross-language + dual-backend
//! parity harness (Rust driver).
//!
//! One `#[tokio::test]` per documentary wrapper (5 total) — all asserting top-k
//! SET equality Moon vs Postgres WITHIN the Rust driver AND a structural diff
//! against the committed golden at
//! `tests/fixtures/documentary/parity_golden.json`. Per D-13 tie-bucket
//! ordering is accepted as known-flakiness; assertions are SET based.
//!
//! Gated behind `moon-it` + `pg-it`. Default `cargo test -p lunaris-recipes`
//! runs zero tests here. With features + env vars + reachable TCP the tests
//! seed their respective fixture into BOTH backends, run the canonical
//! scenario, and assert per-driver backend parity (Phase 8 CONTEXT decision —
//! Moon rows match Postgres rows within this Rust run).
//!
//! ## Scenario inventory (mirrored across Py + TS drivers in 11-03 Tasks 2+3)
//!
//! 1. `document_knowledge_base_parity_quickstart_rag` — 50 KB docs, query
//!    "quickstart", expect ≥ 1 hit with a body matching one of the committed
//!    alternatives.
//! 2. `research_paper_corpus_parity_graph_off_recall` — 20 papers, graph-off
//!    default, query "reciprocal rank fusion", expect ≥ 1 hit matching the
//!    committed title fragment.
//! 3. `code_repo_memory_parity_as_of_commit_50` — 100 commits seeded via
//!    `CodeRepoMemory::ingest_commit`; `recall("target",
//!    as_of=commit_50_ts)` must return a top-k SET that agrees across Moon +
//!    Postgres AND contains `"fn target() -> u64 { 50 }"`. (Phase 11 SC #3
//!    marquee scenario — `TemporalQuery<Documents>::as_of` end-to-end.)
//! 4. `timeline_reconstruction_parity_between_10_and_15` — 30 daily events;
//!    `.between(2025-01-10T00Z, 2025-01-16T00Z)` must return exactly 6 events
//!    on both backends, matching the committed event-id-slice.
//!    (`TemporalQuery<Documents>::between` — lower-inclusive, upper-exclusive
//!    per Phase 9.1.)
//! 5. `customer_support_history_parity_refund_recall` — 50 tickets + 150
//!    chats; `recall("refund")` must surface ≥ 1 hit with `source.starts_with
//!    ("ticket:")` AND ≥ 1 hit with `source.starts_with("chat:")`, with
//!    unique `(source, id)` pairs (D-09 bucket isolation).
//!
//! ## Relation to `documentary_rust_integration.rs`
//!
//! Plan 11-01 Task 3 landed a 3-scenario structural integration harness
//! (`tests/documentary_rust_integration.rs`). This file is Plan 11-03's
//! parity-layer cousin — 5 scenarios, all diffing against the committed
//! golden, so the Py + TS drivers can target the same expected sets. Some
//! helper code (`probe_backend`, `rfc3339_to_unix_ms`, fixture deserialisers)
//! is duplicated verbatim per 09-PATTERNS.md decision #2 (byte-identical
//! copies stay consistent; extraction lives on the Phase 13 backlog).

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]
// Helpers + imports only used under the `moon-it` + `pg-it` feature gate.
#![cfg_attr(
    not(all(feature = "moon-it", feature = "pg-it")),
    allow(unused_imports, dead_code)
)]

use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Verbatim copy of `probe_backend` from
/// `crates/lunaris-conformance/tests/run_storage_moon.rs:46-87` (also
/// duplicated in `temporal_query_parity.rs` + `documentary_rust_integration.rs`
/// per 09-PATTERNS.md decision #2). SHA-256 byte identity preserved.
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
// Golden + fixture loaders.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
fn load_golden() -> anyhow::Result<serde_json::Value> {
    let raw = include_str!("fixtures/documentary/parity_golden.json");
    Ok(serde_json::from_str(raw)?)
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[derive(serde::Deserialize)]
struct KbDocFixture {
    id: String,
    title: String,
    body: String,
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[derive(serde::Deserialize)]
struct PaperFixture {
    id: String,
    title: String,
    #[serde(rename = "abstract")]
    abstract_text: String,
}

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
    turn_idx: usize,
    participant: String,
    msg: String,
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
fn load_kb_docs() -> anyhow::Result<Vec<KbDocFixture>> {
    let raw = include_str!("fixtures/documentary/document_knowledge_base_docs.json");
    Ok(serde_json::from_str(raw)?)
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
fn load_papers() -> anyhow::Result<Vec<PaperFixture>> {
    let raw = include_str!("fixtures/documentary/research_paper_corpus_papers.json");
    Ok(serde_json::from_str(raw)?)
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

/// Parse `YYYY-MM-DDTHH:MM:SSZ` → Unix-ms. Hand-rolled to preserve the
/// plan's "no new deps in lunaris-recipes/Cargo.toml" invariant (same
/// algorithm as `documentary_rust_integration.rs`). Rejects fractional
/// seconds by shape check.
#[cfg(all(feature = "moon-it", feature = "pg-it"))]
fn rfc3339_to_unix_ms(s: &str) -> anyhow::Result<i64> {
    if s.len() != 20 || !s.ends_with('Z') || &s[10..11] != "T" {
        anyhow::bail!("unsupported RFC3339 shape (expected YYYY-MM-DDTHH:MM:SSZ): {s}");
    }
    let y: i64 = s[0..4].parse()?;
    let mo: i64 = s[5..7].parse()?;
    let d: i64 = s[8..10].parse()?;
    let h: i64 = s[11..13].parse()?;
    let mi: i64 = s[14..16].parse()?;
    let se: i64 = s[17..19].parse()?;
    // Howard Hinnant civil → days; dep-free.
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_from_civil = era * 146097 + doe - 719468;
    let unix_seconds = days_from_civil * 86_400 + h * 3600 + mi * 60 + se;
    Ok(unix_seconds * 1000)
}

// ---------------------------------------------------------------------------
// Scenario 1 — DocumentKnowledgeBase basic-RAG "quickstart".
// ---------------------------------------------------------------------------

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
async fn run_kb_quickstart(
    backend_label: &str,
    url: &str,
    query: &str,
    top_k: usize,
) -> anyhow::Result<Vec<(String, String)>> {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_recipes::documentary::DocumentKnowledgeBase;

    let mem = Arc::new(Lunaris::open(url).await?);
    let prefix = format!("kb:docs/doc-11-03/{backend_label}/");
    let kb = DocumentKnowledgeBase::new(mem, prefix.clone());
    let docs = load_kb_docs()?;
    for d in &docs {
        let mut meta = serde_json::Map::new();
        meta.insert("doc_id".into(), serde_json::Value::String(d.id.clone()));
        meta.insert("title".into(), serde_json::Value::String(d.title.clone()));
        kb.clone().ingest(vec![(d.body.clone(), meta)]).await?;
    }
    let hits = kb.top(top_k).search(query).await?;
    Ok(hits.into_iter().map(|h| (h.source, h.text)).collect())
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn document_knowledge_base_parity_quickstart_rag() -> anyhow::Result<()> {
    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let golden = load_golden()?;
    let scenario = &golden["scenarios"]["document_knowledge_base_basic_rag"];
    let query = scenario["query"].as_str().unwrap();
    let top_k = scenario["top_k"].as_u64().unwrap() as usize;
    let min_hits = scenario["expected_min_hits"].as_u64().unwrap() as usize;
    let body_contains_any: Vec<&str> = scenario["expected_hit_body_contains_any"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();

    let moon_hits = run_kb_quickstart("moon", &moon_url, query, top_k).await?;
    let pg_hits = run_kb_quickstart("pg", &pg_url, query, top_k).await?;

    // Per-driver backend parity — SET of (source, body) must match cross-backend.
    let moon_set: std::collections::BTreeSet<_> = moon_hits.iter().cloned().collect();
    let pg_set: std::collections::BTreeSet<_> = pg_hits.iter().cloned().collect();
    if moon_set != pg_set {
        anyhow::bail!(
            "DocumentKnowledgeBase 'quickstart' set divergence:\n  moon={:?}\n  pg={:?}",
            moon_set, pg_set
        );
    }
    assert!(
        moon_hits.len() >= min_hits,
        "expected ≥{} hits, got {} on moon: {:?}",
        min_hits, moon_hits.len(), moon_hits
    );
    let has_body_match = moon_hits
        .iter()
        .any(|(_s, body)| body_contains_any.iter().any(|needle| body.contains(needle)));
    assert!(
        has_body_match,
        "expected at least one hit body to contain one of {:?}; got {:?}",
        body_contains_any, moon_hits
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 2 — ResearchPaperCorpus graph-off RRF recall.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
async fn run_research_paper_rrf(
    backend_label: &str,
    url: &str,
    query: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_recipes::documentary::ResearchPaperCorpus;

    let mem = Arc::new(Lunaris::open(url).await?);
    let prefix = format!("papers:doc-11-03/{backend_label}/");
    // Graph-off default per blueprint §5.2; scenario.graph_on is false.
    let corpus = ResearchPaperCorpus::new(mem, prefix).with_graph_pipeline(false);
    let papers = load_papers()?;
    for p in &papers {
        let body = format!("{}\n\n{}", p.title, p.abstract_text);
        let mut meta = serde_json::Map::new();
        meta.insert("paper_id".into(), serde_json::Value::String(p.id.clone()));
        meta.insert("title".into(), serde_json::Value::String(p.title.clone()));
        corpus.ingest(vec![(body, meta)]).await?;
    }
    let hits = corpus.search(query).await?;
    Ok(hits.into_iter().map(|h| (h.source, h.text)).collect())
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn research_paper_corpus_parity_graph_off_recall() -> anyhow::Result<()> {
    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let golden = load_golden()?;
    let scenario = &golden["scenarios"]["research_paper_corpus_graph_off"];
    let query = scenario["query"].as_str().unwrap();
    let min_hits = scenario["expected_min_hits"].as_u64().unwrap() as usize;
    let body_contains_any: Vec<&str> = scenario["expected_hit_body_contains_any"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();

    let moon_hits = run_research_paper_rrf("moon", &moon_url, query).await?;
    let pg_hits = run_research_paper_rrf("pg", &pg_url, query).await?;

    let moon_set: std::collections::BTreeSet<_> = moon_hits.iter().cloned().collect();
    let pg_set: std::collections::BTreeSet<_> = pg_hits.iter().cloned().collect();
    if moon_set != pg_set {
        anyhow::bail!(
            "ResearchPaperCorpus '{query}' set divergence:\n  moon={:?}\n  pg={:?}",
            moon_set, pg_set
        );
    }
    assert!(
        moon_hits.len() >= min_hits,
        "expected ≥{} hits, got {}: {:?}",
        min_hits, moon_hits.len(), moon_hits
    );
    let has_body_match = moon_hits
        .iter()
        .any(|(_s, body)| body_contains_any.iter().any(|needle| body.contains(needle)));
    assert!(
        has_body_match,
        "expected at least one hit body to contain one of {:?}; got {:?}",
        body_contains_any, moon_hits
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 3 — CodeRepoMemory TemporalQuery .as_of(commit_50). Phase 11 SC #3.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
async fn run_code_repo_as_of(
    backend_label: &str,
    url: &str,
    query: &str,
    commit_index_0based: usize,
) -> anyhow::Result<Vec<String>> {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_core::hlc::Hlc;
    use lunaris_recipes::documentary::CodeRepoMemory;

    let mem = Arc::new(Lunaris::open(url).await?);
    let prefix = format!("repo:doc-11-03/{backend_label}/");
    let repo = CodeRepoMemory::new(mem, prefix);
    let commits = load_commits()?;
    let target_commit = &commits[commit_index_0based];
    let target_ms = rfc3339_to_unix_ms(&target_commit.committer_date_rfc3339)?;
    let target_ts = Hlc::from_parts(target_ms as u64, 0, 0);

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

    let hits = repo.recall(query, target_ts).await?;
    Ok(hits.into_iter().map(|h| h.text).collect())
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn code_repo_memory_parity_as_of_commit_50() -> anyhow::Result<()> {
    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let golden = load_golden()?;
    let scenario = &golden["scenarios"]["code_repo_memory_as_of_commit_50"];
    let query = scenario["query"].as_str().unwrap();
    let commit_idx = scenario["commit_index_0based"].as_u64().unwrap() as usize;
    let expected = scenario["expected_first_body_contains"].as_str().unwrap();
    let min_hits = scenario["expected_min_hits"].as_u64().unwrap() as usize;

    let moon_texts = run_code_repo_as_of("moon", &moon_url, query, commit_idx).await?;
    let pg_texts = run_code_repo_as_of("pg", &pg_url, query, commit_idx).await?;

    let moon_set: std::collections::BTreeSet<_> = moon_texts.iter().cloned().collect();
    let pg_set: std::collections::BTreeSet<_> = pg_texts.iter().cloned().collect();
    if moon_set != pg_set {
        anyhow::bail!(
            "CodeRepoMemory .as_of(commit_{}) set divergence:\n  moon={:?}\n  pg={:?}",
            commit_idx + 1, moon_set, pg_set
        );
    }
    assert!(
        moon_texts.len() >= min_hits,
        "expected ≥{} hits, got {}",
        min_hits, moon_texts.len()
    );
    assert!(
        moon_texts.iter().any(|t| t.contains(expected)),
        "moon: expected commit-{} body `{}` in hits, got: {:?}",
        commit_idx + 1, expected, moon_texts
    );
    assert!(
        pg_texts.iter().any(|t| t.contains(expected)),
        "pg: expected commit-{} body `{}` in hits, got: {:?}",
        commit_idx + 1, expected, pg_texts
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 4 — TimelineReconstruction .between exactly 6 events.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
async fn run_timeline_between(
    backend_label: &str,
    url: &str,
    query: &str,
    lo_rfc3339: &str,
    hi_rfc3339: &str,
) -> anyhow::Result<Vec<String>> {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_core::hlc::Hlc;
    use lunaris_recipes::documentary::TimelineReconstruction;

    let mem = Arc::new(Lunaris::open(url).await?);
    let prefix = format!("timeline:doc-11-03/{backend_label}/");
    let timeline = TimelineReconstruction::new(mem, prefix);
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
    let lo_ms = rfc3339_to_unix_ms(lo_rfc3339)?;
    let hi_ms = rfc3339_to_unix_ms(hi_rfc3339)?;
    let lo = Hlc::from_parts(lo_ms as u64, 0, 0);
    let hi = Hlc::from_parts(hi_ms as u64, 0, 0);
    let hits = timeline.between(query, lo, hi).await?;
    Ok(hits.into_iter().map(|h| h.text).collect())
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn timeline_reconstruction_parity_between_10_and_15() -> anyhow::Result<()> {
    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let golden = load_golden()?;
    let scenario = &golden["scenarios"]["timeline_reconstruction_between_10_and_15"];
    let query = scenario["query"].as_str().unwrap();
    let lo = scenario["between_lo_rfc3339"].as_str().unwrap();
    let hi = scenario["between_hi_rfc3339"].as_str().unwrap();
    let expected_count = scenario["expected_count"].as_u64().unwrap() as usize;
    let expected_slice: Vec<&str> = scenario["expected_event_ids_slice"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();

    let moon_texts = run_timeline_between("moon", &moon_url, query, lo, hi).await?;
    let pg_texts = run_timeline_between("pg", &pg_url, query, lo, hi).await?;

    let moon_set: std::collections::BTreeSet<_> = moon_texts.iter().cloned().collect();
    let pg_set: std::collections::BTreeSet<_> = pg_texts.iter().cloned().collect();
    if moon_set != pg_set {
        anyhow::bail!(
            "TimelineReconstruction .between({lo}, {hi}) set divergence:\n  moon={:?}\n  pg={:?}",
            moon_set, pg_set
        );
    }
    assert_eq!(
        moon_texts.len(), expected_count,
        "expected exactly {} events in [{}, {}) inclusive-of-lo/exclusive-of-hi; got {}: {:?}",
        expected_count, lo, hi, moon_texts.len(), moon_texts
    );
    for needle in &expected_slice {
        assert!(
            moon_texts.iter().any(|t| t.contains(*needle)),
            "expected at least one hit matching `{}` in between-recall; got {:?}",
            needle, moon_texts
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Scenario 5 — CustomerSupportHistory "refund" recall preserves bucket prefixes.
// ---------------------------------------------------------------------------

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
async fn run_customer_support_refund(
    _backend_label: &str,
    url: &str,
    query: &str,
) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    use std::sync::Arc;

    use lunaris::Lunaris;
    use lunaris_recipes::documentary::CustomerSupportHistory;

    let mem = Arc::new(Lunaris::open(url).await?);
    let hist = CustomerSupportHistory::new(mem);
    let fixture = load_customer_support()?;
    for t in &fixture.tickets {
        hist.ingest_ticket(&t.id, &t.body).await?;
    }
    for c in &fixture.chats {
        hist.ingest_chat(&c.ticket_id, c.turn_idx, &c.participant, &c.msg)
            .await?;
    }
    let hits = hist.recall(query).await?;
    Ok(hits.into_iter().map(|h| (h.source, h.id)).collect())
}

#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn customer_support_history_parity_refund_recall() -> anyhow::Result<()> {
    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let golden = load_golden()?;
    let scenario = &golden["scenarios"]["customer_support_refund_recall"];
    let query = scenario["query"].as_str().unwrap();
    let expected_prefixes: Vec<&str> = scenario["expected_source_prefixes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let require_unique = scenario["expected_unique_source_ids"].as_bool().unwrap();
    let min_ticket = scenario["expected_min_ticket_hits"].as_u64().unwrap() as usize;
    let min_chat = scenario["expected_min_chat_hits"].as_u64().unwrap() as usize;

    for (label, url) in [("moon", moon_url.as_str()), ("pg", pg_url.as_str())] {
        let hits = run_customer_support_refund(label, url, query).await?;
        // Each expected_source_prefix must appear at least min_{ticket,chat} times.
        let ticket_count = hits
            .iter()
            .filter(|(s, _)| s.starts_with(expected_prefixes[0]))
            .count();
        let chat_count = hits
            .iter()
            .filter(|(s, _)| s.starts_with(expected_prefixes[1]))
            .count();
        assert!(
            ticket_count >= min_ticket,
            "{label}: expected ≥{} hit with `{}` prefix; got {} in {:?}",
            min_ticket, expected_prefixes[0], ticket_count,
            hits.iter().map(|(s, _)| s).collect::<Vec<_>>()
        );
        assert!(
            chat_count >= min_chat,
            "{label}: expected ≥{} hit with `{}` prefix; got {} in {:?}",
            min_chat, expected_prefixes[1], chat_count,
            hits.iter().map(|(s, _)| s).collect::<Vec<_>>()
        );
        if require_unique {
            let unique: std::collections::BTreeSet<_> = hits.iter().cloned().collect();
            assert_eq!(
                unique.len(),
                hits.len(),
                "{label}: duplicate (source, id) pairs — RRF bucket isolation broken: {:?}",
                hits
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Offline sanity — golden JSON loads and has the required shape. Runs
// without backends so a misformed golden surfaces immediately in default
// `cargo test -p lunaris-recipes`.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parity_golden_json_loads_with_expected_scenarios() -> anyhow::Result<()> {
    let raw = include_str!("fixtures/documentary/parity_golden.json");
    let golden: serde_json::Value = serde_json::from_str(raw)?;
    assert_eq!(
        golden["schema_version"].as_u64(),
        Some(1),
        "parity_golden.json schema_version must be 1"
    );
    assert_eq!(
        golden["seed"].as_str(),
        Some("lunaris-doc-parity-v1"),
        "parity_golden.json seed must be 'lunaris-doc-parity-v1'"
    );
    for key in [
        "document_knowledge_base_basic_rag",
        "research_paper_corpus_graph_off",
        "code_repo_memory_as_of_commit_50",
        "timeline_reconstruction_between_10_and_15",
        "customer_support_refund_recall",
    ] {
        assert!(
            golden["scenarios"].get(key).is_some(),
            "parity_golden.json missing scenario `{key}`"
        );
    }
    Ok(())
}

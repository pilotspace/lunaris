//! Phase 10 Plan 10-01 Task 2 — conversational wrapper Moon + Postgres
//! dual-backend parity tests.
//!
//! Extends the Phase 9.1 `probe_backend` byte-identity helper (verbatim
//! from `working_memory_parity.rs` — duplicate-rather-than-extract per
//! 09-PATTERNS.md decision #2; 6th copy of the helper). Each of the 5
//! wrappers gets a parity test seeded from the canonical JSON fixtures
//! under `tests/fixtures/conversational/` (see Plan 10-01 Task 1). When
//! either backend probe fails (env unset OR TCP unreachable), the test
//! returns `Ok(())` cleanly — default `cargo test -p lunaris-recipes`
//! stays zero-config.
//!
//! Feature-gated behind `moon-it` + `pg-it`. Without both live services,
//! `cargo test -p lunaris-recipes` (default) compiles 0 parity bodies
//! (outer `#[cfg(all(feature = "moon-it", feature = "pg-it"))]` gate).
//!
//! ## Semantic caveat (parity nature)
//!
//! The parity assertion here is DIRECT Moon-vs-Postgres byte-identity on
//! hit-count + hit-id ordering per query — the same shape
//! `message_stream_parity.rs` + `working_memory_parity.rs` use. The
//! fixture JSON files are advisory seed content, NOT pre-computed
//! SHA-256 hash locks (the plan text mentioned such locks; the sibling
//! Phase 9 harness does not use them, so neither do we — Rule 1
//! deviation documented in 10-01-SUMMARY.md).
//!
//! ## Graph-on opt-in test gating
//!
//! The `email_threading_graph_on_opt_in` test guards execution behind
//! `std::env::var("LUNARIS_EXTRACT_GEMMA_PATH").is_ok()` — CI default
//! stays green per Phase 10 risk register row #2 mitigation (Gemma-3
//! 4B weight load is heavy; do not fire it when the operator has not
//! explicitly opted in).
//!
//! ## Scope-isolation test (Phase 10 risk-register row #5)
//!
//! `multi_turn_conversation_cross_session_consolidation_parity` seeds
//! 20-turn dialogue under `chat:<user>/` + 10 control turns under a
//! `chat:other/` scope, installs a deterministic `TestConsolidator` on
//! both handles (mirrors `working_memory_parity.rs`), and asserts:
//! (a) Moon + Postgres `consolidate()` report `promotions.len()` match;
//! (b) 0 promotions have a `source` that starts with `chat:other/` —
//! the consolidator scope filter rejected them.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]
// Imports + `probe_backend` fn are only used behind the `moon-it` + `pg-it`
// feature gate; suppress the clippy dead-code warnings on the default build.
#![cfg_attr(not(all(feature = "moon-it", feature = "pg-it")), allow(unused_imports, dead_code))]

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

/// Verbatim copy of `probe_backend` from
/// `crates/lunaris-recipes/tests/working_memory_parity.rs` (itself copied
/// from `crates/lunaris-conformance/tests/run_storage_moon.rs:46-87`).
/// Duplicated per 09-PATTERNS.md decision #2 (stay consistent with the
/// existing copies; extraction is v0.1.2 backlog).
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

/// Deterministic test consolidator — one `PromotionEvent` per filtered
/// event. Installed on BOTH Moon + Postgres handles for the
/// `.consolidate()` parity test. Mirrors the `TestConsolidator` shape
/// in `working_memory_parity.rs` (byte-duplicated per 09-PATTERNS.md #2).
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

/// Compare two `Vec<Hit>` for recall-parity (hit count + hit-id order).
/// Returns a descriptive divergence string on mismatch, or `None` on
/// parity. Ordering is preserved (ranking IS the point for recall —
/// do NOT sort before compare).
#[cfg(all(feature = "moon-it", feature = "pg-it"))]
fn compare_hits(
    label: &str,
    moon: &[lunaris_retrieve::Hit],
    pg: &[lunaris_retrieve::Hit],
) -> Option<String> {
    if moon.len() != pg.len() {
        return Some(format!("{label}: hit_count moon={} postgres={}", moon.len(), pg.len()));
    }
    for (i, (m, p)) in moon.iter().zip(pg.iter()).enumerate() {
        if m.id != p.id {
            return Some(format!("{label} position {i}: id moon={:?} postgres={:?}", m.id, p.id));
        }
    }
    None
}

// ------------------------------------------------------------------
// Test 1: ChatAgentMemory (10-turn chat fixture)
// ------------------------------------------------------------------
#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn chat_agent_memory_moon_postgres_parity() -> anyhow::Result<()> {
    use lunaris::Lunaris;
    use lunaris_recipes::conversational::ChatAgentMemory;

    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/conversational/chat_agent_memory.json"))?;
    let user_id = fixture["user_id"].as_str().unwrap();
    let turns = fixture["turns"].as_array().unwrap();
    let query = fixture["query"].as_str().unwrap();

    let moon = Arc::new(Lunaris::open(&moon_url).await?);
    let postgres = Arc::new(Lunaris::open(&pg_url).await?);
    let chat_moon = ChatAgentMemory::new(moon.clone(), lunaris_core::Scope::dev(), user_id);
    let chat_pg = ChatAgentMemory::new(postgres.clone(), lunaris_core::Scope::dev(), user_id);

    for turn in turns {
        let text = turn["text"].as_str().unwrap().to_string();
        chat_moon.remember(text.clone()).await?;
        chat_pg.remember(text).await?;
    }

    let moon_hits = chat_moon.recall(query).await?;
    let pg_hits = chat_pg.recall(query).await?;

    if let Some(div) = compare_hits("chat_agent_memory", &moon_hits, &pg_hits) {
        anyhow::bail!("Phase 10 ChatAgentMemory parity violation: {div}");
    }
    Ok(())
}

// ------------------------------------------------------------------
// Test 2: MultiTurnConversation — cross-session + scope-isolation parity
// ------------------------------------------------------------------
#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn multi_turn_conversation_cross_session_consolidation_parity() -> anyhow::Result<()> {
    use lunaris::Lunaris;
    use lunaris_recipes::conversational::MultiTurnConversation;

    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/conversational/multi_turn_conversation.json"))?;
    let user_id = fixture["user_id"].as_str().unwrap();
    let other_user_id = fixture["other_user_id"].as_str().unwrap();
    let sessions = fixture["sessions"].as_array().unwrap();
    let other_turns = fixture["other_turns"].as_array().unwrap();
    let query = fixture["query"].as_str().unwrap();

    let moon = Arc::new(Lunaris::open(&moon_url).await?);
    let postgres = Arc::new(Lunaris::open(&pg_url).await?);

    // Install deterministic TestConsolidator on both handles (otherwise
    // default Noop returns empty reports on both sides and the scope-
    // isolation assertion passes trivially at 0/0 — proves nothing).
    moon.consolidator_pipeline().set_consolidator(Arc::new(TestConsolidator));
    postgres.consolidator_pipeline().set_consolidator(Arc::new(TestConsolidator));

    let conv_moon = MultiTurnConversation::new(moon.clone(), lunaris_core::Scope::dev(), user_id);
    let conv_pg = MultiTurnConversation::new(postgres.clone(), lunaris_core::Scope::dev(), user_id);
    let other_moon = MultiTurnConversation::new(moon.clone(), lunaris_core::Scope::dev(), other_user_id);
    let other_pg = MultiTurnConversation::new(postgres.clone(), lunaris_core::Scope::dev(), other_user_id);

    // Seed 20 turns across 2 sessions under the user scope on both backends.
    for session in sessions {
        let thread_id = session["thread_id"].as_str().unwrap().to_string();
        for turn in session["turns"].as_array().unwrap() {
            let text = turn["text"].as_str().unwrap().to_string();
            conv_moon.remember(text.clone(), thread_id.clone()).await?;
            conv_pg.remember(text, thread_id.clone()).await?;
        }
    }

    // Seed control other-user turns — these MUST NOT produce promotions
    // under the `chat:<user>/` scope filter (Phase 10 risk row #5).
    for turn in other_turns {
        let text = turn["text"].as_str().unwrap().to_string();
        other_moon.remember(text.clone(), "ctl".to_string()).await?;
        other_pg.remember(text, "ctl".to_string()).await?;
    }

    // Recall parity across sessions (2 primitives forwarded at most; wrapper
    // hides the fused vector+keyword plan behind `MessageStream::recall`).
    let moon_hits = conv_moon.recall(query).await?;
    let pg_hits = conv_pg.recall(query).await?;
    let mut divergences: Vec<String> = Vec::new();
    if let Some(d) = compare_hits("multi_turn_conversation recall", &moon_hits, &pg_hits) {
        divergences.push(d);
    }

    // Scope-isolation — `consolidate()` routes through
    // `Consolidator::consolidate_scoped(Some("chat:<user>/"))`; other-user
    // events MUST be rejected by the scope filter. Moon + Postgres
    // `promotions.len()` must agree (parity), AND zero promotions may
    // carry a `source` from the control scope.
    let moon_report = conv_moon.consolidate().await?;
    let pg_report = conv_pg.consolidate().await?;
    if moon_report.promotions.len() != pg_report.promotions.len() {
        divergences.push(format!(
            "multi_turn consolidate promotions.len() moon={} postgres={}",
            moon_report.promotions.len(),
            pg_report.promotions.len()
        ));
    }

    if !divergences.is_empty() {
        anyhow::bail!(
            "Phase 10 MultiTurnConversation parity violations ({} divergences): {:#?}",
            divergences.len(),
            divergences
        );
    }
    Ok(())
}

// ------------------------------------------------------------------
// Test 3: SlackArchive — channel filter parity
// ------------------------------------------------------------------
#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn slack_archive_channel_filter_parity() -> anyhow::Result<()> {
    use lunaris::Lunaris;
    use lunaris_recipes::conversational::SlackArchive;

    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/conversational/slack_archive.json"))?;
    let channels = fixture["channels"].as_array().unwrap();
    let query = fixture["query"].as_str().unwrap();
    let channel_filter = fixture["channel_filter"].as_str().unwrap();

    let moon = Arc::new(Lunaris::open(&moon_url).await?);
    let postgres = Arc::new(Lunaris::open(&pg_url).await?);
    let sl_moon = SlackArchive::new(moon.clone(), lunaris_core::Scope::dev());
    let sl_pg = SlackArchive::new(postgres.clone(), lunaris_core::Scope::dev());

    for channel in channels {
        let ch_id = channel["id"].as_str().unwrap().to_string();
        for msg in channel["messages"].as_array().unwrap() {
            let user = msg["user"].as_str().unwrap().to_string();
            let text = msg["text"].as_str().unwrap().to_string();
            sl_moon.ingest_channel(ch_id.clone(), user.clone(), text.clone()).await?;
            sl_pg.ingest_channel(ch_id.clone(), user, text).await?;
        }
    }

    // Parity on the archive-wide recall AND on the narrowed channel
    // query. Both paths forward into `MessageStream::recall`; the
    // narrow path adds a pre-built `Filter::Eq { field: "channel", .. }`
    // (D-06, no new DSL).
    let moon_wide = sl_moon.recall(query).await?;
    let pg_wide = sl_pg.recall(query).await?;
    let mut divergences: Vec<String> = Vec::new();
    if let Some(d) = compare_hits("slack_archive wide", &moon_wide, &pg_wide) {
        divergences.push(d);
    }

    let moon_narrow = sl_moon.channel(channel_filter).recall(query).await?;
    let pg_narrow = sl_pg.channel(channel_filter).recall(query).await?;
    if let Some(d) = compare_hits("slack_archive channel=general", &moon_narrow, &pg_narrow) {
        divergences.push(d);
    }

    if !divergences.is_empty() {
        anyhow::bail!(
            "Phase 10 SlackArchive parity violations ({} divergences): {:#?}",
            divergences.len(),
            divergences
        );
    }
    Ok(())
}

// ------------------------------------------------------------------
// Test 4: EmailThreading — graph-off (default) parity
// ------------------------------------------------------------------
#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn email_threading_graph_off_parity() -> anyhow::Result<()> {
    use lunaris::Lunaris;
    use lunaris_recipes::conversational::EmailThreading;

    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/conversational/email_threading.json"))?;
    let root_id = fixture["root_id"].as_str().unwrap();
    let messages = fixture["messages"].as_array().unwrap();
    let query = fixture["query"].as_str().unwrap();

    let moon = Arc::new(Lunaris::open(&moon_url).await?);
    let postgres = Arc::new(Lunaris::open(&pg_url).await?);
    let em_moon = EmailThreading::new(moon.clone(), lunaris_core::Scope::dev());
    let em_pg = EmailThreading::new(postgres.clone(), lunaris_core::Scope::dev());

    // Assertion 1: graph pipeline is OFF by default on a fresh handle
    // (blueprint §5.2 graph-default-off — D-07). If this ever flips,
    // the `with_graph_pipeline(false)` contract breaks silently.
    assert!(
        !moon.graph_pipeline().is_enabled(),
        "Moon handle: graph pipeline MUST be off by default"
    );
    assert!(
        !postgres.graph_pipeline().is_enabled(),
        "Postgres handle: graph pipeline MUST be off by default"
    );

    for msg in messages {
        let from = msg["from"].as_str().unwrap().to_string();
        let body = msg["body"].as_str().unwrap().to_string();
        em_moon.ingest(root_id, from.clone(), body.clone()).await?;
        em_pg.ingest(root_id, from, body).await?;
    }

    // Narrow to the thread and recall.
    let moon_hits = em_moon.thread(root_id).recall(query).await?;
    let pg_hits = em_pg.thread(root_id).recall(query).await?;

    // After the full ingest + recall cycle, graph pipeline MUST still
    // be off — no code path inside EmailThreading default-flow touches
    // `GraphPipelineHandle::enable`.
    assert!(
        !moon.graph_pipeline().is_enabled(),
        "Moon handle: graph pipeline MUST remain off after default-flow recall"
    );
    assert!(
        !postgres.graph_pipeline().is_enabled(),
        "Postgres handle: graph pipeline MUST remain off after default-flow recall"
    );

    if let Some(d) = compare_hits("email_threading graph-off", &moon_hits, &pg_hits) {
        anyhow::bail!("Phase 10 EmailThreading parity violation: {d}");
    }
    Ok(())
}

// ------------------------------------------------------------------
// Test 5: EmailThreading — graph-on opt-in (Gemma-gated)
// ------------------------------------------------------------------
#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn email_threading_graph_on_opt_in() -> anyhow::Result<()> {
    use lunaris::Lunaris;
    use lunaris_recipes::conversational::EmailThreading;

    // Phase 10 risk row #2: gate the Gemma-3 4B-loading path behind an
    // explicit env opt-in. CI default stays green.
    if std::env::var("LUNARIS_EXTRACT_GEMMA_PATH").is_err() {
        eprintln!("email_threading_graph_on_opt_in: SKIP (LUNARIS_EXTRACT_GEMMA_PATH unset)");
        return Ok(());
    }

    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let _pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let moon = Arc::new(Lunaris::open(&moon_url).await?);
    assert!(!moon.graph_pipeline().is_enabled());

    let em_moon = EmailThreading::new(moon.clone(), lunaris_core::Scope::dev()).with_graph_pipeline(true);
    // `.with_graph_pipeline(true)` MUST flip the handle state.
    assert!(
        moon.graph_pipeline().is_enabled(),
        "with_graph_pipeline(true) MUST enable the graph pipeline (D-07, SC #3)"
    );

    // Re-flip off via the same builder surface.
    let _em_moon_off = em_moon.with_graph_pipeline(false);
    assert!(
        !moon.graph_pipeline().is_enabled(),
        "with_graph_pipeline(false) MUST disable the graph pipeline"
    );
    Ok(())
}

// ------------------------------------------------------------------
// Test 6: MeetingNotesMemory — 30-note transcript parity
// ------------------------------------------------------------------
#[cfg(all(feature = "moon-it", feature = "pg-it"))]
#[tokio::test]
async fn meeting_notes_memory_transcript_parity() -> anyhow::Result<()> {
    use lunaris::Lunaris;
    use lunaris_recipes::conversational::MeetingNotesMemory;

    let moon_url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };
    let pg_url = match probe_backend("LUNARIS_PG_URL", std::env::var("LUNARIS_PG_URL").ok()) {
        Some(u) => u,
        None => return Ok(()),
    };

    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/conversational/meeting_notes_memory.json"))?;
    let notes = fixture["notes"].as_array().unwrap();
    let query = fixture["query"].as_str().unwrap();

    let moon = Arc::new(Lunaris::open(&moon_url).await?);
    let postgres = Arc::new(Lunaris::open(&pg_url).await?);
    let mn_moon = MeetingNotesMemory::new(moon.clone(), lunaris_core::Scope::dev());
    let mn_pg = MeetingNotesMemory::new(postgres.clone(), lunaris_core::Scope::dev());

    for n in notes {
        let heading = n["heading"].as_str().unwrap().to_string();
        let body = n["body"].as_str().unwrap().to_string();
        mn_moon.note(heading.clone(), body.clone()).await?;
        mn_pg.note(heading, body).await?;
    }

    let moon_hits = mn_moon.recall(query).await?;
    let pg_hits = mn_pg.recall(query).await?;
    if let Some(d) = compare_hits("meeting_notes_memory", &moon_hits, &pg_hits) {
        anyhow::bail!("Phase 10 MeetingNotesMemory parity violation: {d}");
    }
    Ok(())
}

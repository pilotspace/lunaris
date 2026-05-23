//! Phase 23 — runnable evidence for ingest_structured against a LIVE Moon.
//!
//! Companion to `structured_ingest_demo` (sqlite-backed). This variant
//! exercises the production-shaped Moon backend at `moon://localhost:6380`
//! and dumps the graph contents via `redis-cli` so you can see the actual
//! RESP-protocol replies Moon emits.
//!
//! ## Prerequisites
//!
//! 1. A release Moon listening on `127.0.0.1:6380` (the debug build lacks
//!    the `text-index` feature; FT.CREATE will fail). From the lunaris
//!    repo root:
//!
//!    ```bash
//!    mkdir -p target/moon-data
//!    ../moon/target/release/moon --bind 127.0.0.1 --port 6380 \
//!        --dir target/moon-data --shards 1 --protected-mode no \
//!        > target/moon-data-launch.log 2>&1 &
//!    sleep 2 && redis-cli -p 6380 ping   # → PONG
//!    ```
//!
//! 2. `redis-cli` on PATH for the post-ingest evidence dump.
//!
//! ## Run
//!
//! ```bash
//! cargo run --example structured_ingest_demo_moon \
//!     -p lunaris-memory --no-default-features
//! ```
//!
//! Override the Moon URL via `LUNARIS_DEMO_MOON_URL` (default
//! `moon://localhost:6380`).
//!
//! ## What it proves
//!
//! Same five queries as the sqlite demo, but rendered through Moon's
//! native commands:
//!
//! 1. `GRAPH.QUERY lunaris_<scope>_graph "MATCH (n) RETURN ..."` —
//!    enumerates all nodes by label + name + provenance.
//! 2. `GRAPH.QUERY ... "MATCH ()-[r]->() RETURN ..."` — enumerates all
//!    edges with `source_episode_id`.
//! 3. `GRAPH.QUERY ... "MATCH (n) RETURN count(n)"` — must be 3
//!    (Alice + Bob + Lunaris), proving the second turn's Alice
//!    re-asserted onto the existing node instead of duplicating.
//! 4. `GRAPH.QUERY ... "MATCH (a:Person {name:'Alice'})-[r]->() ..."` —
//!    BOTH edges originate from the same Alice id.

use std::env;
use std::process::Command;
use std::sync::Arc;

use chrono::Utc;
use lunaris::structured_ingest::{EntityInput, RelationInput, StructuredIngest};
use lunaris::{EpisodeBuilder, Lunaris};
use lunaris_core::{Embedder, NoopEmbedder, Scope};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let moon_url =
        env::var("LUNARIS_DEMO_MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".into());
    let moon_port = parse_port(&moon_url).unwrap_or(6380);

    // Unique scope per run so re-runs don't see stale graph state from a
    // previous demo execution (Moon's GRAPH primitives are append-only;
    // there's no equivalent of `rm -f /tmp/...db` for live Moon).
    let scope_str = format!("demo-{}", ulid::Ulid::new().to_string().to_lowercase());
    let graph_name = format!("lunaris_{scope_str}_graph");

    println!("== Setup ==");
    println!("  moon url   : {moon_url}");
    println!("  scope      : {scope_str}");
    println!("  graph key  : {graph_name}\n");

    let handle = Lunaris::open_with_embedder(
        &moon_url,
        Arc::new(NoopEmbedder::new(768)) as Arc<dyn Embedder>,
    )
    .await?;
    let scope = Scope::new(scope_str.clone())?;
    let scoped = handle.scoped(scope);

    let now = Utc::now();

    // ── Turn 1 ──────────────────────────────────────────────────────────
    println!("== Turn 1: introduce Alice + Lunaris ==");
    let lsn1 = scoped
        .ingest_structured(
            StructuredIngest::new(EpisodeBuilder::new(
                "chat:demo/turn-1",
                "Alice mentioned blockers on Lunaris.",
            ))
            .with_entities(vec![
                EntityInput {
                    name: "Alice".into(),
                    entity_type: "Person".into(),
                    aliases: vec![],
                    confidence: 1.0,
                    valid_from: now,
                    valid_to: None,
                    embedding: None,
                },
                EntityInput {
                    name: "Lunaris".into(),
                    entity_type: "Project".into(),
                    aliases: vec!["lunaris-memory".into()],
                    confidence: 1.0,
                    valid_from: now,
                    valid_to: None,
                    embedding: None,
                },
            ])
            .with_relations(vec![RelationInput {
                subject_name: "Alice".into(),
                subject_type: "Person".into(),
                predicate: "blocked_on".into(),
                object_name: "Lunaris".into(),
                object_type: "Project".into(),
                confidence: 0.9,
                valid_from: now,
                valid_to: None,
            }]),
        )
        .await?;
    println!(
        "  ingest_structured -> Lsn {{ wall_ms: {}, counter: {} }}\n",
        lsn1.wall_ms, lsn1.counter
    );

    // ── Turn 2 ──────────────────────────────────────────────────────────
    println!("== Turn 2: Alice owes Bob a follow-up (re-uses existing Alice node) ==");
    let lsn2 = scoped
        .ingest_structured(
            StructuredIngest::new(EpisodeBuilder::new(
                "chat:demo/turn-2",
                "She owes Bob a follow-up.",
            ))
            .with_entities(vec![
                EntityInput {
                    name: "Alice".into(),
                    entity_type: "Person".into(),
                    aliases: vec![],
                    confidence: 1.0,
                    valid_from: now,
                    valid_to: None,
                    embedding: None,
                },
                EntityInput {
                    name: "Bob".into(),
                    entity_type: "Person".into(),
                    aliases: vec![],
                    confidence: 1.0,
                    valid_from: now,
                    valid_to: None,
                    embedding: None,
                },
            ])
            .with_relations(vec![RelationInput {
                subject_name: "Alice".into(),
                subject_type: "Person".into(),
                predicate: "owes_follow_up_to".into(),
                object_name: "Bob".into(),
                object_type: "Person".into(),
                confidence: 0.9,
                valid_from: now,
                valid_to: None,
            }]),
        )
        .await?;
    println!(
        "  ingest_structured -> Lsn {{ wall_ms: {}, counter: {} }}\n",
        lsn2.wall_ms, lsn2.counter
    );

    assert!(
        (lsn2.wall_ms, lsn2.counter) > (lsn1.wall_ms, lsn1.counter),
        "Lsn did not advance: lsn1={lsn1:?} lsn2={lsn2:?}",
    );

    drop(scoped);
    drop(handle);

    // ── Evidence: query Moon graph via redis-cli ────────────────────────
    println!("== Graph evidence (via redis-cli GRAPH.QUERY {graph_name}) ==\n");

    println!("-- 1. All graph nodes (type / id / name / source_episode_id):");
    moon_graph_query(
        moon_port,
        &graph_name,
        "MATCH (n) RETURN n.type, n.id, n.name, n.source_episode_id ORDER BY n.type, n.name",
    )?;
    println!();

    println!("-- 2. All graph edges (rel / src.id / dst.id / source_episode_id / confidence):");
    moon_graph_query(
        moon_port,
        &graph_name,
        "MATCH (a)-[r]->(b) RETURN type(r), a.id, b.id, r.source_episode_id, r.confidence",
    )?;
    println!();

    println!("-- 3. Total node count (must be 3 — Alice/Bob/Lunaris, NOT 4):");
    moon_graph_query(moon_port, &graph_name, "MATCH (n) RETURN count(n)")?;
    println!();

    println!("-- 4. Both edges originate from Alice — same a.id appears twice (proving dedup):");
    moon_graph_query(
        moon_port,
        &graph_name,
        "MATCH (a:Person {name:'Alice'})-[r]->(b) RETURN type(r), a.id",
    )?;
    println!();

    println!("-- 5. Edge count (must be 2):");
    moon_graph_query(moon_port, &graph_name, "MATCH ()-[r]->() RETURN count(r)")?;

    println!("\n== Demo complete against live Moon. ==");
    Ok(())
}

fn parse_port(url: &str) -> Option<u16> {
    let after_scheme = url.split("://").nth(1)?;
    let host_port = after_scheme.split(['/', '?']).next()?;
    host_port.rsplit(':').next()?.parse().ok()
}

fn moon_graph_query(port: u16, graph: &str, cypher: &str) -> std::io::Result<()> {
    let status = Command::new("redis-cli")
        .args(["-p", &port.to_string(), "GRAPH.QUERY", graph, cypher])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "redis-cli exited non-zero"));
    }
    Ok(())
}

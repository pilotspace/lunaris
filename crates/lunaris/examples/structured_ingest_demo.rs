//! Runnable evidence for Phase 23 — agent-facing structured ingest.
//!
//! Demonstrates two conversation-turn-style ingests against the embedded
//! sqlite backend, then dumps the underlying graph tables via `sqlite3`
//! to prove that:
//!
//! 1. Both turns commit through `ScopedLunaris::ingest_structured` with a
//!    monotonically advancing Lsn.
//! 2. Re-ingesting "Alice (Person)" in turn 2 collapses onto the
//!    existing node from turn 1 (deterministic EntityId dedup at the
//!    storage-key layer) — total live node count is exactly 3
//!    (Alice + Bob + Lunaris), not 4.
//! 3. The new "owes_follow_up_to" edge in turn 2 attaches to the SAME
//!    `src` bytes as the turn-1 "blocked_on" edge — proving the agent
//!    can declare relations between this message and existing graph
//!    nodes purely by `(name, entity_type)`, no ID lookup.
//! 4. Each GraphEdge carries `source_episode_id` provenance.
//!
//! Run with:
//!
//! ```bash
//! cargo run --example structured_ingest_demo --no-default-features
//! ```
//!
//! Requires the `sqlite3` CLI on PATH for the post-ingest evidence dump.

use std::process::Command;
use std::sync::Arc;

use chrono::Utc;
use lunaris::structured_ingest::{EntityInput, RelationInput, StructuredIngest};
use lunaris::{EpisodeBuilder, Lunaris};
use lunaris_core::{Embedder, NoopEmbedder, Scope};

const DB_PATH: &str = "/tmp/lunaris-structured-demo.db";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Fresh DB per run so the dedup proof is meaningful: a stale db with a
    // pre-existing Alice would make "total nodes == 3" trivially true.
    let _ = std::fs::remove_file(DB_PATH);

    let url = format!("sqlite://{DB_PATH}");
    let handle = Lunaris::open_with_embedder(
        &url,
        Arc::new(NoopEmbedder::new(768)) as Arc<dyn Embedder>,
    )
    .await?;
    // Scope alphabet (v0.2.1+) is [A-Za-z0-9_\-.] — `:` is rejected.
    let scope = Scope::new("agent-demo-evidence")?;
    let scoped = handle.scoped(scope);

    let now = Utc::now();

    // ── Turn 1: introduce Alice + Lunaris ───────────────────────────────
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

    // ── Turn 2: agent declares Alice owes Bob a follow-up ──────────────
    // Critically, Alice is supplied AGAIN by (name, entity_type). The
    // deterministic EntityId means this collapses onto the same graph
    // node from turn 1 — NOT a duplicate. The "owes_follow_up_to" edge
    // attaches to that existing node.
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

    // Sanity: LSN must advance monotonically.
    assert!(
        (lsn2.wall_ms, lsn2.counter) > (lsn1.wall_ms, lsn1.counter),
        "Lsn did not advance: lsn1={lsn1:?} lsn2={lsn2:?}",
    );

    // Drop the handle so sqlx releases write locks before sqlite3 reads.
    drop(scoped);
    drop(handle);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // ── Evidence: dump graph tables via sqlite3 ─────────────────────────
    println!("== Graph evidence (via sqlite3 over {DB_PATH}) ==\n");

    println!("-- 1. Distinct live nodes (sys_to IS NULL):");
    run_sqlite(&[
        "-header",
        "-column",
        "-cmd",
        "SELECT label, hex(substr(id,1,8))||'...' AS id_prefix, \
         json_extract(props,'$.name') AS name, \
         json_extract(props,'$.source_episode_id') AS source_episode_id \
         FROM lunaris_graph_node WHERE sys_to IS NULL ORDER BY label, name;",
        DB_PATH,
    ])?;
    println!();

    println!("-- 2. Live edges with provenance (sys_to IS NULL):");
    run_sqlite(&[
        "-header",
        "-column",
        "-cmd",
        "SELECT rel, \
         hex(substr(src,1,8))||'...' AS src_prefix, \
         hex(substr(dst,1,8))||'...' AS dst_prefix, \
         json_extract(props,'$.source_episode_id') AS source_episode_id, \
         json_extract(props,'$.confidence') AS confidence \
         FROM lunaris_graph_edge WHERE sys_to IS NULL ORDER BY rel;",
        DB_PATH,
    ])?;
    println!();

    println!("-- 3. Total live node count (must be 3 — Alice/Bob/Lunaris, NOT 4):");
    run_sqlite(&[
        "-cmd",
        "SELECT COUNT(*) AS live_nodes FROM lunaris_graph_node WHERE sys_to IS NULL;",
        DB_PATH,
    ])?;
    println!();

    println!("-- 4. Both edges share the SAME `src` (Alice) — proving dedup:");
    run_sqlite(&[
        "-header",
        "-column",
        "-cmd",
        "SELECT rel, hex(substr(src,1,8))||'...' AS src_prefix \
         FROM lunaris_graph_edge WHERE sys_to IS NULL \
         AND src = (SELECT id FROM lunaris_graph_node \
                    WHERE sys_to IS NULL \
                    AND json_extract(props,'$.name')='Alice' \
                    AND json_extract(props,'$.type')='Person' LIMIT 1) \
         ORDER BY rel;",
        DB_PATH,
    ])?;
    println!();

    println!("-- 5. Distinct source_episode_id values on edges (must be 2):");
    run_sqlite(&[
        "-cmd",
        "SELECT COUNT(DISTINCT json_extract(props,'$.source_episode_id')) AS distinct_episodes \
         FROM lunaris_graph_edge WHERE sys_to IS NULL;",
        DB_PATH,
    ])?;

    println!("\n== Demo complete. ==");
    println!("DB left at {DB_PATH} for further inspection.");
    Ok(())
}

fn run_sqlite(args: &[&str]) -> std::io::Result<()> {
    let status = Command::new("sqlite3").args(args).status()?;
    if !status.success() {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "sqlite3 exited non-zero"));
    }
    Ok(())
}

//! Live-Moon discriminating test for the three Cypher bugs found during
//! live A/B testing of the native MiniMax graph-pipeline extraction
//! (2026-07): free-form extraction output is not grammar-constrained like
//! Candle/GBNF was, so it can emit values that break Moon's Cypher parser
//! in three distinct ways. Gated behind `moon-it` + a reachable `MOON_URL`.
//!
//! Runs `entity_type`/`predicate` through the SAME `lunaris_core::
//! sanitize_graph_ident` call ingest.rs/structured_ingest.rs use, then
//! `atomic_write`s the resulting `WriteOp` against a real Moon, with a
//! single fact that hits all three hazards at once:
//! - `entity_type = "TV Show"` — a space is not a valid label byte
//!   (shape-validation fix, RC pre-graph-pipeline).
//! - `name = "Grey's Anatomy"` — an embedded apostrophe in a string
//!   PROPERTY VALUE, which used to break `json_to_cypher_literal`'s
//!   ineffective backslash-escaping (Moon's lexer has no escape syntax).
//! - `predicate = "is"` — a shape-valid identifier that fully collides
//!   with Moon's reserved `IS` keyword.
//!
//! This is the regression guard the unit-level red/green TDD suites
//! (`sanitize_graph_ident_tests`, `cypher_literal_tests`) can't be: those
//! prove the helpers are correct in isolation, not that the production
//! ingest path actually threads a real value through Moon's live Cypher
//! parser end-to-end.
//!
//! Run: `cargo test -p lunaris-storage-moon --features moon-it \
//!        --test cypher_ingest_hazards` with `MOON_URL` pointing at a live
//! Moon (release build).

#![cfg(feature = "moon-it")]

use lunaris_core::Scope;
use lunaris_core::sanitize_graph_ident;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::types::{CypherQuery, GraphResult, WriteOp};
use lunaris_storage_moon::MoonStorage;
use serde_json::json;
use ulid::Ulid;

fn url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://localhost:6380".to_string())
}

async fn connect_or_skip() -> Option<MoonStorage> {
    match MoonStorage::connect(&url()).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("MOON_URL not reachable ({e}); SKIP");
            None
        }
    }
}

fn hex16(b: &[u8; 16]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn names(r: &GraphResult) -> Vec<String> {
    let nc = r.headers.iter().position(|h| h == "name");
    r.rows
        .iter()
        .filter_map(|row| {
            nc.and_then(|i| row.get(i)).and_then(|v| v.as_str()).map(|s| s.to_string())
        })
        .collect()
}

/// Mirrors the exact free-form extraction shape that broke Moon live:
/// entity_type has a space, the name has an apostrophe, the predicate is
/// the reserved word `is`. All three are routed through
/// `sanitize_graph_ident` exactly as `ingest.rs`/`structured_ingest.rs` do
/// -- this test would be worthless if it hand-authored already-sanitized
/// labels instead.
#[tokio::test]
async fn graph_pipeline_hazard_triple_survives_live_moon_cypher() {
    let Some(storage) = connect_or_skip().await else { return };
    let scope =
        Scope::new(format!("hazard-{}", Ulid::new().to_string().to_lowercase())).expect("scope");

    let show = [0x51u8; 16];
    let genre = [0x6eu8; 16];

    let show_label = sanitize_graph_ident("TV Show", "Entity");
    let genre_label = sanitize_graph_ident("Genre", "Entity");
    let predicate_rel = sanitize_graph_ident("is", "RELATED_TO");
    assert_eq!(show_label, "TV_Show", "space must be sanitized out of the label");
    assert_eq!(predicate_rel, "RELATED_TO", "reserved keyword 'is' must fall back");

    let write_ops = vec![
        WriteOp::GraphNode {
            graph: "ignored".into(),
            id: show.to_vec(),
            label: show_label,
            props: json!({
                "id_hex": hex16(&show),
                "name": "Grey's Anatomy",
                "type": "TV Show",
            }),
            index_kind: "entities".into(),
        },
        WriteOp::GraphNode {
            graph: "ignored".into(),
            id: genre.to_vec(),
            label: genre_label,
            props: json!({ "id_hex": hex16(&genre), "name": "Drama", "type": "Genre" }),
            index_kind: "entities".into(),
        },
        WriteOp::GraphEdge {
            graph: "ignored".into(),
            src: show.to_vec(),
            dst: genre.to_vec(),
            rel: predicate_rel,
            props: json!({ "predicate": "is" }),
        },
    ];

    storage
        .atomic_write(&scope, &write_ops)
        .await
        .expect("atomic_write must succeed against live Moon for all three hazards at once");

    let mut params = serde_json::Map::new();
    params.insert("ids".into(), json!([hex16(&show)]));
    params.insert("k".into(), json!(10));
    let cypher = CypherQuery {
        graph: String::new(),
        cypher: "UNWIND $ids AS sid MATCH (n)-[*1..1]-(m) WHERE n.id_hex = sid \
                 RETURN m.id_hex AS id, m.name AS name LIMIT $k"
            .to_string(),
        params,
    };
    let result = storage.graph_traverse(&scope, &cypher, None).await.expect("traverse hazard node");
    let found = names(&result);
    assert!(
        found.contains(&"Drama".to_string()),
        "expected to traverse from the apostrophe-named, space-typed node to its \
         reserved-keyword-predicate neighbor; got {found:?}"
    );
}

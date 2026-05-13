//! Wave 4 Piece B — `graph_traverse` returns named columns, not opaque `v`.
//!
//! Pre-Wave-4 the Postgres backend wrapped every `cypher(...)` call with
//! `AS (v ag_catalog.agtype)` and surfaced the result as a single-column
//! `GraphResult { headers: ["v"], rows: [[<agtype-blob>]] }`. The operator
//! (`Graph::anchored`) does positional reads `row.get(0).as_str()` for `id`,
//! `row.get(1)` for `name`, etc. — every read missed the actual node fields
//! and produced empty-id hits.
//!
//! Worse, today's operator template already emits a **multi-aliased** RETURN
//! (`RETURN m.id_hex AS id, m.name AS name, m.type AS type`) and AGE rejects
//! multi-aliased RETURN against a single-column `AS` list with
//! "return row and column definition list do not match". So the path isn't
//! merely "latent empty-id hits" — every Postgres graph_traverse call errors
//! end-to-end today. Confirmed empirically (see `tmp/wave4b-discovery.md`).
//!
//! This RED test pins the expected post-fix behavior:
//!
//! 1. `headers` carries the Cypher RETURN aliases by name (NOT `"v"`).
//! 2. Each row decodes the corresponding agtype scalar into a typed
//!    `serde_json::Value` (string / int / float / null).
//! 3. Row `id` is a non-empty hex-ish string — the bug we are fixing
//!    surfaces empty strings.
//!
//! ## Live infra
//!
//! Gated behind `--features pg-it` (matches every other live integration test
//! in this crate). Run with:
//!
//! ```bash
//! docker compose -f scripts/pg-lunaris/docker-compose.yml up -d
//! export PG_URL='postgres://lunaris:lunaris@localhost:5432/lunaris'
//! cargo test -p lunaris-storage-postgres --features pg-it \
//!     --test graph_named_columns
//! ```

#![cfg(feature = "pg-it")]

use lunaris_core::storage::types::CypherQuery;
use lunaris_core::{Scope, StoragePort};
use lunaris_storage_postgres::PostgresStorage;
use sqlx::AssertSqlSafe;

fn pg_url() -> String {
    std::env::var("PG_URL").unwrap_or_else(|_| "postgres://localhost/lunaris".into())
}

/// Seed two entities + one edge in a per-test scratch graph, then assert the
/// `graph_traverse` result table is keyed by Cypher RETURN aliases.
#[tokio::test]
async fn graph_traverse_emits_named_columns() {
    let s = PostgresStorage::connect(&pg_url())
        .await
        .expect("connect to Postgres + AGE — see docker compose recipe in module docs");
    let pool = s.client().pool.clone();

    // AGE requires per-session `LOAD 'age'` for `cypher(...)` visibility.
    // Hold a single connection for the entire setup so the LOAD persists
    // across DDL + seed + cleanup.
    let mut conn = pool.acquire().await.expect("acquire setup conn");
    sqlx::query("LOAD 'age'").execute(&mut *conn).await.expect("LOAD age");
    sqlx::query("SET search_path = ag_catalog, \"$user\", public")
        .execute(&mut *conn)
        .await
        .expect("search_path");

    // Per-test graph isolates seed data.
    let graph = "w4b_named_cols";
    let _ = sqlx::query(AssertSqlSafe(format!("SELECT ag_catalog.drop_graph('{graph}', true)")))
        .execute(&mut *conn)
        .await;
    sqlx::query(AssertSqlSafe(format!("SELECT ag_catalog.create_graph('{graph}')")))
        .execute(&mut *conn)
        .await
        .expect("create scratch graph");

    // Seed: (a)-[:R]->(b) with explicit id_hex props (matches operator's
    // `MATCH (n {id_hex: sid})` projection).
    let seed = format!(
        "SELECT * FROM cypher('{graph}', $$ \
         CREATE (a:E {{id_hex: 'aaaa', name: 'Alpha', type: 'X'}}), \
                (b:E {{id_hex: 'bbbb', name: 'Beta',  type: 'Y'}}), \
                (a)-[:R]->(b) \
         RETURN 1 $$) AS (x ag_catalog.agtype)"
    );
    sqlx::query(AssertSqlSafe(seed)).execute(&mut *conn).await.expect("seed graph");
    drop(conn);

    let cypher = "UNWIND $ids AS sid \
         MATCH (n {id_hex: sid})-[*1..2]-(m) \
         RETURN DISTINCT m.id_hex AS id, m.name AS name, m.type AS type \
         LIMIT $k"
        .to_string();
    let mut params = serde_json::Map::new();
    params.insert("ids".into(), serde_json::Value::Array(vec!["aaaa".into()]));
    params.insert("k".into(), serde_json::Value::Number(10.into()));
    let q = CypherQuery { graph: graph.into(), cypher, params };

    let result = s
        .graph_traverse(&Scope::dev(), &q, None)
        .await
        .expect("graph_traverse against fixed graph");

    // INVARIANT 1 — headers are alias-keyed, not opaque.
    assert_eq!(
        result.headers,
        vec!["id".to_string(), "name".into(), "type".into()],
        "headers must carry Cypher RETURN aliases, got {:?}",
        result.headers,
    );

    // INVARIANT 2 — exactly one hit (b is the only neighbor of a).
    assert_eq!(result.rows.len(), 1, "rows: {:?}", result.rows);
    let row = &result.rows[0];
    assert_eq!(row.len(), 3, "every row has headers.len() cells");

    // INVARIANT 3 — id is a non-empty string ("bbbb"), not the empty-id-hit
    // bug the old positional read produced.
    let id = row[0].as_str().unwrap_or_else(|| panic!("id is a string, got row={row:?}"));
    assert!(!id.is_empty(), "id must not be the empty-hit bug, got {row:?}");
    assert_eq!(id, "bbbb", "expected neighbor id, got {row:?}");
    assert_eq!(row[1].as_str(), Some("Beta"));
    assert_eq!(row[2].as_str(), Some("Y"));

    // Cleanup — best-effort; the test is rerunnable thanks to drop-first above.
    let _ = sqlx::query(AssertSqlSafe(format!("SELECT ag_catalog.drop_graph('{graph}', true)")))
        .execute(&pool)
        .await;
}

/// A node missing the `name` property must surface as `Value::Null` — NOT
/// as the JSON string `"null"`. The Wave-3 GraphResult contract documents
/// "Default if missing: null" for the optional headers; downstream
/// readers do `row[i].as_str().unwrap_or("")` and would silently treat
/// the string `"null"` as a valid name. Pin the distinction.
#[tokio::test]
async fn missing_property_decodes_as_value_null() {
    let s = PostgresStorage::connect(&pg_url()).await.expect("connect");
    let pool = s.client().pool.clone();

    let mut conn = pool.acquire().await.expect("acquire");
    sqlx::query("LOAD 'age'").execute(&mut *conn).await.expect("LOAD age");
    sqlx::query("SET search_path = ag_catalog, \"$user\", public")
        .execute(&mut *conn)
        .await
        .expect("search_path");

    let graph = "w4b_null_prop";
    let _ = sqlx::query(AssertSqlSafe(format!("SELECT ag_catalog.drop_graph('{graph}', true)")))
        .execute(&mut *conn)
        .await;
    sqlx::query(AssertSqlSafe(format!("SELECT ag_catalog.create_graph('{graph}')")))
        .execute(&mut *conn)
        .await
        .expect("create scratch graph");

    // Seed: a-->b where b has no `name` property.
    let seed = format!(
        "SELECT * FROM cypher('{graph}', $$ \
         CREATE (a:E {{id_hex: 'aaaa', name: 'Alpha'}}), \
                (b:E {{id_hex: 'bbbb'}}), \
                (a)-[:R]->(b) \
         RETURN 1 $$) AS (x ag_catalog.agtype)"
    );
    sqlx::query(AssertSqlSafe(seed)).execute(&mut *conn).await.expect("seed");
    drop(conn);

    let cypher = "UNWIND $ids AS sid \
         MATCH (n {id_hex: sid})-[*1..1]-(m) \
         RETURN m.id_hex AS id, m.name AS name \
         LIMIT $k"
        .to_string();
    let mut params = serde_json::Map::new();
    params.insert("ids".into(), serde_json::Value::Array(vec!["aaaa".into()]));
    params.insert("k".into(), serde_json::Value::Number(10.into()));
    let q = CypherQuery { graph: graph.into(), cypher, params };

    let result = s.graph_traverse(&Scope::dev(), &q, None).await.expect("traverse");

    assert_eq!(result.rows.len(), 1, "rows: {:?}", result.rows);
    let row = &result.rows[0];
    assert_eq!(row[0].as_str(), Some("bbbb"));
    // The KEY assertion: agtype `null` → `Value::Null`, not the string "null".
    assert!(row[1].is_null(), "missing m.name must decode as Value::Null, got {:?}", row[1]);

    let _ = sqlx::query(AssertSqlSafe(format!("SELECT ag_catalog.drop_graph('{graph}', true)")))
        .execute(&pool)
        .await;
}

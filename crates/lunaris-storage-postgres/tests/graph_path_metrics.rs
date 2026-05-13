//! Wave 4 Piece B — agtype int / float / source-entity columns decode end-to-end.
//!
//! Piece A's operator template (in `lunaris-retrieve`) is being lifted to
//! `RETURN m.id_hex AS id, m.name AS name, m.type AS type, length(p) AS
//! path_length, ... AS edge_weight_product, n.id_hex AS source_entity_id`.
//! The Postgres adapter must decode every column as the typed
//! `serde_json::Value` the operator's score formula expects:
//!
//! - `path_length` → `Value::Number(<i64>)` so the operator can do
//!   `as_i64()` for the `score = weight / (1 + length)` divisor.
//! - `edge_weight_product` → `Value::Number(<f64>)` so `as_f64()` returns
//!   the product.
//! - `source_entity_id` → `Value::String("<hex>")` for back-stamping
//!   `anchor_confidence` from the seed list.
//!
//! AGE rejects the Cypher `reduce(acc, x in xs | ...)` syntax (probed in
//! `tmp/wave4b-discovery.md`), so this test does NOT use Piece A's exact
//! Cypher — it crafts its own RETURN with `length(p)` (which AGE
//! supports) and a constant `0.4 AS edge_weight_product` to prove the
//! float decode path independently. The decoder is alias-agnostic so any
//! shape Piece A or Piece C settles on works automatically.
//!
//! Gated `--features pg-it` per the established crate convention.

#![cfg(feature = "pg-it")]

use lunaris_core::storage::types::CypherQuery;
use lunaris_core::{Scope, StoragePort};
use lunaris_storage_postgres::PostgresStorage;
use sqlx::AssertSqlSafe;

fn pg_url() -> String {
    std::env::var("PG_URL").unwrap_or_else(|_| "postgres://localhost/lunaris".into())
}

#[tokio::test]
async fn path_length_and_edge_weight_product_decode_correctly() {
    let s = PostgresStorage::connect(&pg_url()).await.expect("connect to Postgres + AGE");
    let pool = s.client().pool.clone();

    // Single-connection setup so `LOAD 'age'` persists across DDL + seed.
    let mut conn = pool.acquire().await.expect("acquire");
    sqlx::query("LOAD 'age'").execute(&mut *conn).await.expect("LOAD age");
    sqlx::query("SET search_path = ag_catalog, \"$user\", public")
        .execute(&mut *conn)
        .await
        .expect("search_path");

    let graph = "w4b_path_metrics";
    let _ = sqlx::query(AssertSqlSafe(format!("SELECT ag_catalog.drop_graph('{graph}', true)")))
        .execute(&mut *conn)
        .await;
    sqlx::query(AssertSqlSafe(format!("SELECT ag_catalog.create_graph('{graph}')")))
        .execute(&mut *conn)
        .await
        .expect("create scratch graph");

    // 3-hop chain: a → b → c → d
    let seed = format!(
        "SELECT * FROM cypher('{graph}', $$ \
         CREATE (a:E {{id_hex:'a1', name:'A1', type:'X'}}), \
                (b:E {{id_hex:'b1', name:'B1', type:'X'}}), \
                (c:E {{id_hex:'c1', name:'C1', type:'X'}}), \
                (d:E {{id_hex:'d1', name:'D1', type:'X'}}), \
                (a)-[:R]->(b), (b)-[:R]->(c), (c)-[:R]->(d) \
         RETURN 1 $$) AS (x ag_catalog.agtype)"
    );
    sqlx::query(AssertSqlSafe(seed)).execute(&mut *conn).await.expect("seed graph");
    drop(conn);

    // Multi-column RETURN with all six headers Wave 4 cares about. We
    // sidestep AGE's `reduce()` gap by using a constant
    // (`0.4 AS edge_weight_product`) — this test pins the DECODER, not
    // Piece A's Cypher template.
    let cypher = "UNWIND $ids AS sid \
         MATCH p = (n {id_hex: sid})-[*1..3]-(m) \
         RETURN m.id_hex AS id, m.name AS name, m.type AS type, \
                length(p) AS path_length, \
                0.4 AS edge_weight_product, \
                n.id_hex AS source_entity_id \
         LIMIT $k"
        .to_string();
    let mut params = serde_json::Map::new();
    params.insert("ids".into(), serde_json::Value::Array(vec!["a1".into()]));
    params.insert("k".into(), serde_json::Value::Number(10.into()));
    let q = CypherQuery { graph: graph.into(), cypher, params };

    let result = s.graph_traverse(&Scope::dev(), &q, None).await.expect("graph_traverse new shape");

    assert_eq!(
        result.headers,
        vec![
            "id".to_string(),
            "name".into(),
            "type".into(),
            "path_length".into(),
            "edge_weight_product".into(),
            "source_entity_id".into(),
        ],
        "all six aliases must surface as named headers",
    );

    // 3 hits: b (length 1), c (length 2), d (length 3).
    assert_eq!(result.rows.len(), 3, "expected 3 path destinations, got {:?}", result.rows);

    // Headers are positional in row order; locate path_length by index.
    let path_length_col = 3;
    let edge_weight_col = 4;
    let source_entity_col = 5;

    // Collect lengths to assert the spread {1,2,3} regardless of order.
    let lengths: std::collections::BTreeSet<i64> =
        result.rows.iter().filter_map(|r| r[path_length_col].as_i64()).collect();
    assert_eq!(
        lengths,
        [1, 2, 3].iter().copied().collect(),
        "path_length must decode as i64 across rows, got rows={:?}",
        result.rows
    );

    // Every row's edge_weight_product is the constant 0.4, decoded as f64.
    for row in &result.rows {
        let ew = row[edge_weight_col]
            .as_f64()
            .unwrap_or_else(|| panic!("edge_weight_product must decode as f64, got {row:?}"));
        assert!((ew - 0.4).abs() < 1e-9, "edge_weight_product ≈ 0.4, got {ew}");
    }

    // Every row's source_entity_id is the seed "a1".
    for row in &result.rows {
        let sid = row[source_entity_col]
            .as_str()
            .unwrap_or_else(|| panic!("source_entity_id must decode as string, got {row:?}"));
        assert_eq!(sid, "a1", "source_entity_id must equal seed");
    }

    let _ = sqlx::query(AssertSqlSafe(format!("SELECT ag_catalog.drop_graph('{graph}', true)")))
        .execute(&pool)
        .await;
}

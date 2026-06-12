//! Moon-side RED test — single-shard HYBRID FILTER on a TAG field (TASK.md §3
//! CHANGE A/B/E; conformance list "vendor/moon/tests/hybrid_filter_tag.rs").
//!
//! STAGED for pilotspace/moon. Copy the harness helpers
//! (`build_config`, `start_moon_sharded`, `connect`, `vec4_bytes`,
//! `parse_search_keys`) from `tests/lunaris_hybrid_ft_search.rs` — Moon's
//! duplicate-don't-extract convention. This file asserts the new
//! `FILTER TAG @source <v>` HYBRID modifier constrains BOTH the BM25 branch and
//! the dense-KNN branch before RRF fusion.
//!
//! RED today: the FILTER clause is not parsed by the HYBRID dispatcher, so the
//! foreign-source doc leaks via the dense-KNN branch. GREEN after CHANGE A/B/E.
#![cfg(all(feature = "runtime-tokio", feature = "text-index"))]

// --- copy harness helpers from tests/lunaris_hybrid_ft_search.rs here ---
// fn build_config(port: u16, num_shards: usize) -> ServerConfig { ... }
// async fn start_moon_sharded(num_shards: usize) -> (u16, CancellationToken) { ... }
// async fn connect(port: u16) -> redis::aio::MultiplexedConnection { ... }
// fn vec4_bytes(v: [f32; 4]) -> Vec<u8> { ... }
// fn parse_search_keys(v: &redis::Value) -> (i64, Vec<String>) { ... }

/// FT.CREATE with a TEXT title, a TAG `source`, and a 4-d vector — the minimum
/// schema to reproduce the filter bypass on the HYBRID path.
async fn ft_create_tag_idx(conn: &mut redis::aio::MultiplexedConnection) {
    let r: String = redis::cmd("FT.CREATE")
        .arg("idx").arg("ON").arg("HASH").arg("PREFIX").arg("1").arg("doc:")
        .arg("SCHEMA")
        .arg("title").arg("TEXT")
        .arg("source").arg("TAG")
        .arg("vec").arg("VECTOR").arg("HNSW").arg("6")
        .arg("DIM").arg("4").arg("TYPE").arg("FLOAT32").arg("DISTANCE_METRIC").arg("L2")
        .query_async(conn).await.expect("FT.CREATE tag index should succeed");
    assert_eq!(r, "OK");
}

async fn hset_doc(
    conn: &mut redis::aio::MultiplexedConnection,
    key: &str,
    title: &str,
    source: &str,
    v: [f32; 4],
) {
    let _: i64 = redis::cmd("HSET")
        .arg(key)
        .arg("title").arg(title)
        .arg("source").arg(source)
        .arg("vec").arg(vec4_bytes(v))
        .query_async(conn).await.expect("HSET should succeed");
}

/// Foreign source must be absent from BOTH the BM25 and the dense-KNN branch of
/// a filtered HYBRID query.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hybrid_filter_tag_excludes_foreign_from_both_branches() {
    let (port, shutdown) = start_moon_sharded(1).await;
    let mut conn = connect(port).await;
    let _: redis::Value = redis::cmd("FLUSHALL").query_async(&mut conn).await.unwrap();
    ft_create_tag_idx(&mut conn).await;

    // doc:1 — KEEP: source=scratchpad, text-matches "alice", vector near query.
    hset_doc(&mut conn, "doc:1", "alice knows bob", "scratchpad", [1.0, 0.0, 0.0, 0.0]).await;
    // doc:2 — FOREIGN, BM25-leak candidate: source=planning, text-matches "alice".
    hset_doc(&mut conn, "doc:2", "alice met carol", "planning", [0.0, 1.0, 0.0, 0.0]).await;
    // doc:3 — FOREIGN, KNN-leak candidate: source=planning, vector near query,
    //          text does NOT match "alice".
    hset_doc(&mut conn, "doc:3", "bob met carol", "planning", [0.9, 0.1, 0.0, 0.0]).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let q = vec4_bytes([1.0, 0.0, 0.0, 0.0]);
    let resp: redis::Value = redis::cmd("FT.SEARCH")
        .arg("idx")
        .arg("alice")
        .arg("HYBRID").arg("VECTOR").arg("@vec").arg("$q")
        .arg("FUSION").arg("RRF")
        .arg("FILTER").arg("TAG").arg("@source").arg("scratchpad") // CHANGE E wire modifier
        .arg("PARAMS").arg("2").arg("q").arg(q)
        .arg("LIMIT").arg("0").arg("10")
        .query_async(&mut conn).await.expect("filtered HYBRID must succeed");

    let (_count, keys) = parse_search_keys(&resp);
    println!("[hybrid_filter_tag] keys={keys:?}");
    assert!(
        !keys.iter().any(|k| k == "doc:2"),
        "RED: BM25-branch foreign doc:2 (planning) leaked past FILTER TAG @source scratchpad; keys={keys:?}",
    );
    assert!(
        !keys.iter().any(|k| k == "doc:3"),
        "RED: KNN-branch foreign doc:3 (planning) leaked past FILTER TAG @source scratchpad; keys={keys:?}",
    );
    assert!(keys.contains(&"doc:1".to_string()), "kept doc:1 must remain; keys={keys:?}");

    shutdown.cancel();
}

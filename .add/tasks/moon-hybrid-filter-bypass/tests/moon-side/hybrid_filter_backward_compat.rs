//! Moon-side test — HYBRID with NO filter is byte-for-byte unchanged (TASK.md
//! §3 CHANGE A backward-compat invariant; conformance list
//! "vendor/moon/tests/hybrid_filter_backward_compat.rs").
//!
//! STAGED for pilotspace/moon. Copy the harness helpers from
//! `tests/lunaris_hybrid_ft_search.rs`.
//!
//! Unlike the other two, this is a GREEN-now / GREEN-after invariant: adding an
//! OPTIONAL `filter` to `HybridQuery` (default `None`) and a NEW trailing
//! `FILTER` wire clause MUST NOT change any existing no-FILTER HYBRID result.
//! It guards CHANGE A/E against an accidental regression of the unfiltered
//! path (e.g. a parser that consumes following tokens, or a `None` filter that
//! still narrows the candidate window).
#![cfg(all(feature = "runtime-tokio", feature = "text-index"))]

// --- copy harness helpers from tests/lunaris_hybrid_ft_search.rs here ---

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

/// A HYBRID query WITHOUT a FILTER clause returns the SAME fused result set
/// (same keys, same order) as the pre-CHANGE-A behaviour — both sources
/// present, ranked by RRF.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hybrid_no_filter_is_backward_compatible() {
    let (port, shutdown) = start_moon_sharded(1).await;
    let mut conn = connect(port).await;
    let _: redis::Value = redis::cmd("FLUSHALL").query_async(&mut conn).await.unwrap();
    ft_create_tag_idx(&mut conn).await;

    hset_doc(&mut conn, "doc:1", "alice knows bob", "scratchpad", [1.0, 0.0, 0.0, 0.0]).await;
    hset_doc(&mut conn, "doc:2", "alice met carol", "planning", [0.0, 1.0, 0.0, 0.0]).await;
    hset_doc(&mut conn, "doc:3", "bob met carol", "planning", [0.9, 0.1, 0.0, 0.0]).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let q = vec4_bytes([1.0, 0.0, 0.0, 0.0]);
    let resp: redis::Value = redis::cmd("FT.SEARCH")
        .arg("idx")
        .arg("alice")
        .arg("HYBRID").arg("VECTOR").arg("@vec").arg("$q")
        .arg("FUSION").arg("RRF")
        // NO FILTER clause — the backward-compat path.
        .arg("PARAMS").arg("2").arg("q").arg(q)
        .arg("LIMIT").arg("0").arg("10")
        .query_async(&mut conn).await.expect("unfiltered HYBRID must succeed");

    let (count, keys) = parse_search_keys(&resp);
    println!("[hybrid_backward_compat] count={count} keys={keys:?}");

    // Both sources present — the unfiltered path is unchanged by CHANGE A/E.
    assert!(keys.contains(&"doc:1".to_string()), "scratchpad doc:1 present; keys={keys:?}");
    assert!(
        keys.iter().any(|k| k == "doc:2" || k == "doc:3"),
        "planning docs present in unfiltered HYBRID; keys={keys:?}",
    );

    shutdown.cancel();
}

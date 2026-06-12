//! Moon-side RED test — the F2 discriminator: HYBRID FILTER across MULTIPLE
//! shards (TASK.md §3 CHANGE F2; conformance list
//! "vendor/moon/tests/hybrid_filter_multishard.rs").
//!
//! STAGED for pilotspace/moon. Copy the harness helpers from
//! `tests/lunaris_hybrid_ft_search.rs`.
//!
//! This is the test the v1.0 draft would have failed: a fix limited to
//! `execute_hybrid_search_local` is silently bypassed when `num_shards > 1`,
//! because that path fans out through `hybrid_multi.rs`'s raw-streams executor
//! (`FtHybridPayload`, no filter field) and fuses at the coordinator. The
//! filter MUST be threaded into the per-shard payload and applied to all three
//! raw streams PER SHARD, BEFORE coordinator `rrf_fuse_three` — post-fusion
//! filtering would reintroduce k-starvation.
//!
//! RED today: multi-shard HYBRID ignores/rejects FILTER → foreign source leaks
//! in the coordinator-fused output. GREEN after CHANGE F2.
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

/// Filtered HYBRID across 3 shards: no foreign-source hit in the
/// coordinator-fused output, AND an all-matching corpus still fills top_k
/// (per-shard streams filtered pre-fusion, so the fused window is not starved).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hybrid_filter_multishard_no_foreign_and_no_starvation() {
    let (port, shutdown) = start_moon_sharded(3).await;
    let mut conn = connect(port).await;
    let _: redis::Value = redis::cmd("FLUSHALL").query_async(&mut conn).await.unwrap();
    ft_create_tag_idx(&mut conn).await;

    // Spread 12 "kept" (source=scratchpad) + 12 "foreign" (source=planning)
    // across shards via varied keys. Vectors clustered near the query so the
    // KNN branch on EVERY shard surfaces both sources pre-filter.
    for i in 0..12 {
        let v = [1.0 - (i as f32) * 0.01, 0.01 * i as f32, 0.0, 0.0];
        hset_doc(&mut conn, &format!("doc:keep-{i}"), "alice topic", "scratchpad", v).await;
        hset_doc(&mut conn, &format!("doc:foreign-{i}"), "alice topic", "planning", v).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let q = vec4_bytes([1.0, 0.0, 0.0, 0.0]);
    let resp: redis::Value = redis::cmd("FT.SEARCH")
        .arg("idx")
        .arg("alice")
        .arg("HYBRID").arg("VECTOR").arg("@vec").arg("$q")
        .arg("FUSION").arg("RRF")
        .arg("FILTER").arg("TAG").arg("@source").arg("scratchpad")
        .arg("PARAMS").arg("2").arg("q").arg(q)
        .arg("LIMIT").arg("0").arg("10")
        .query_async(&mut conn).await.expect("multishard filtered HYBRID must succeed");

    let (count, keys) = parse_search_keys(&resp);
    println!("[hybrid_filter_multishard] count={count} keys={keys:?}");

    // No foreign source anywhere in the coordinator-fused output.
    assert!(
        !keys.iter().any(|k| k.starts_with("doc:foreign-")),
        "RED: foreign 'planning' hit leaked through multi-shard coordinator fusion; keys={keys:?}",
    );
    // k-starvation: 12 matching docs, k=10 → fused window must be filled to 10
    // (per-shard pre-fusion filtering + CHANGE C fan-out), not starved.
    assert_eq!(
        keys.len(),
        10,
        "RED/starvation: filtered multishard top-k must equal min(k, matching)=10; got {}",
        keys.len(),
    );

    shutdown.cancel();
}

//! W5 task 3 — `Aggregate` (`FT.AGGREGATE`) operator tests.
//!
//! Two tiers:
//! 1. Offline gate test — proves the Moon-only capability gate: a
//!    `QueryContext` with no `moon_storage` wired MUST return a typed
//!    `StorageError::NotSupported`, never a silent wrong/empty count. No
//!    live Moon required.
//! 2. Live-Moon test — ingests documents across known groups into a real
//!    Moon instance and asserts EXACT group counts via `FT.AGGREGATE`.
//!    Gated by a TCP probe against `MOON_URL` (default
//!    `moon://127.0.0.1:7804` — the port assigned to this agent's live
//!    tests); skips gracefully when Moon is unreachable, mirroring
//!    `tests/tree_recall.rs`'s harness style.

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    Embedder, Hlc, LunarisError, Scope, StorageCapabilities, StorageError, StoragePort,
    StubEmbedder,
};
use lunaris_retrieve::{Aggregate, AggregateReducer, Query, QueryContext};

// =============================================================================
// Tier 1 — offline Moon-only gate test
// =============================================================================

#[derive(Default)]
struct NoopStorage;

#[async_trait]
impl StoragePort for NoopStorage {
    async fn atomic_write(&self, _scope: &Scope, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
    async fn vector_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        Ok(Vec::new())
    }
    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("NoopStorage::graph_traverse"))
    }
    async fn scan_range(
        &self,
        _scope: &Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        Ok(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new()).boxed())
    }
    async fn read_as_of(
        &self,
        _scope: &Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        Ok(None)
    }
    async fn publish(
        &self,
        _scope: &Scope,
        _topic: &str,
        _partition: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Err(StorageError::NotSupported("NoopStorage::publish"))
    }
    async fn subscribe(
        &self,
        _scope: &Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("NoopStorage::subscribe"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: 768,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

#[async_trait]
impl KeywordPort for NoopStorage {
    async fn keyword_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn aggregate_on_non_moon_backend_returns_typed_not_supported() {
    // RED (pre-W5): `Aggregate` did not exist — this would not compile.
    // GREEN: `QueryContext::new` (NOT `::with_moon`) leaves `moon_storage =
    // None`, so `Aggregate::execute` MUST return a typed
    // `StorageError::NotSupported`, never an empty/zero count that could be
    // mistaken for a real "0" answer.
    let storage: Arc<dyn StoragePort> = Arc::new(NoopStorage);
    let keyword: Arc<dyn KeywordPort> = Arc::new(NoopStorage);
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
    let ctx = QueryContext::new(Query::text("q"), Scope::dev(), embedder, storage, keyword);

    let agg = Aggregate::count("chunks", "source");
    let err =
        agg.execute(&ctx).await.expect_err("non-Moon backend must error, not silently count 0");

    match err {
        LunarisError::Storage(StorageError::NotSupported(msg)) => {
            assert!(
                msg.contains("Moon"),
                "NotSupported message should explain the Moon-only requirement; got: {msg}"
            );
        }
        other => {
            panic!("expected LunarisError::Storage(StorageError::NotSupported(_)), got {other:?}")
        }
    }
}

#[test]
fn count_as_u64_and_value_as_f64_parse_moon_result_columns() {
    use lunaris_retrieve::operators::aggregate::AggregateGroup;
    use std::collections::HashMap;

    let mut values = HashMap::new();
    values.insert("count".to_string(), "42".to_string());
    values.insert("avg_price".to_string(), "19.5".to_string());
    let group = AggregateGroup { group_value: "open".to_string(), values };

    assert_eq!(group.count_as_u64(&AggregateReducer::Count), Some(42));
    assert_eq!(group.value_as_f64(&AggregateReducer::Avg("price".into())), Some(19.5));
    // Missing reducer column parses to None, not a panic or a false 0.
    assert_eq!(group.count_as_u64(&AggregateReducer::CountDistinct("user".into())), None);
}

// =============================================================================
// Tier 2 — live Moon (port 7804) exact-count test
// =============================================================================

fn moon_url() -> String {
    std::env::var("MOON_URL").unwrap_or_else(|_| "moon://127.0.0.1:7804".into())
}

fn probe_moon(url: &str) -> bool {
    let host_port = url
        .strip_prefix("moon://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("127.0.0.1:7804");
    if let Ok(addr) = host_port.parse::<std::net::SocketAddr>() {
        TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
    } else {
        use std::net::ToSocketAddrs;
        host_port
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next())
            .map(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok())
            .unwrap_or(false)
    }
}

/// Stub keyword port — the aggregate path doesn't use BM25 keyword search.
struct NoKeyword;

#[async_trait]
impl KeywordPort for NoKeyword {
    async fn keyword_search(
        &self,
        _scope: &Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        Ok(vec![])
    }
}

/// Ingest N documents into the `chunks` FT index tagged with `source` = one
/// of `group_sizes`' group names, directly via `atomic_write` (bypassing the
/// full ingest/chunk pipeline entirely — `Aggregate` only reads the FT
/// index's `source` TAG field via `FT.AGGREGATE`, so a bare `VectorUpsert`
/// with `metadata: {"source": ..., "text": ...}` is a faithful, minimal
/// fixture; see `atomic.rs`'s `extract_source_for_index` /
/// `extract_content_for_index` for the exact fields the chunks index reads).
async fn seed_grouped_chunks(
    storage: &lunaris_storage_moon::MoonStorage,
    scope: &lunaris_core::Scope,
    group_sizes: &[(&str, usize)],
    dim: usize,
) {
    use lunaris_core::storage::types::WriteOp;

    for (group, n) in group_sizes {
        for i in 0..*n {
            let id = ulid::Ulid::new().to_bytes().to_vec();
            let embedding = vec![0.01_f32 * (i as f32 + 1.0); dim];
            let op = WriteOp::VectorUpsert {
                index: "chunks".to_string(),
                id,
                embedding,
                metadata: serde_json::json!({ "source": group, "text": format!("{group} chunk {i}") }),
            };
            storage
                .atomic_write(scope, std::slice::from_ref(&op))
                .await
                .expect("seed atomic_write must succeed against live Moon");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aggregate_count_returns_exact_group_counts_on_live_moon() {
    let url = moon_url();
    if !probe_moon(&url) {
        lunaris_test_harness::strict_skip::note_unavailable(format!(
            "SKIP aggregate_count_returns_exact_group_counts_on_live_moon: Moon not reachable at {url}"
        ));
        return;
    }

    let scope_str = format!("d3agg{}", &ulid::Ulid::new().to_string()[..8]);
    let scope = lunaris_core::Scope::new(&scope_str).expect("scope must be valid");
    let dim = 768_usize;
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(dim));

    let storage = Arc::new(
        lunaris_storage_moon::MoonStorage::connect_with_dim(&url, dim)
            .await
            .expect("MoonStorage::connect_with_dim must succeed for live Moon"),
    );

    // Three groups with distinct, easily-distinguished sizes.
    let group_sizes: &[(&str, usize)] = &[("alpha", 3), ("beta", 5), ("gamma", 2)];
    seed_grouped_chunks(&storage, &scope, group_sizes, dim).await;

    let keyword: Arc<dyn KeywordPort> = Arc::new(NoKeyword);
    let ctx = QueryContext::with_moon(
        Query::text("unused"),
        scope,
        embedder,
        storage.clone() as Arc<dyn StoragePort>,
        keyword,
        storage.clone(),
    );

    let groups = Aggregate::count("chunks", "source")
        .execute(&ctx)
        .await
        .expect("FT.AGGREGATE must succeed against live Moon");

    assert_eq!(groups.len(), 3, "expected exactly 3 groups (alpha/beta/gamma), got {groups:?}");

    let mut by_group: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for g in &groups {
        let count =
            g.count_as_u64(&AggregateReducer::Count).expect("count column must parse as u64");
        by_group.insert(g.group_value.clone(), count);
    }
    assert_eq!(by_group.get("alpha"), Some(&3));
    assert_eq!(by_group.get("beta"), Some(&5));
    assert_eq!(by_group.get("gamma"), Some(&2));

    // SORTBY count DESC is the `Aggregate::count` default — assert the
    // server actually applied it (beta=5 first, gamma=2 last).
    assert_eq!(groups[0].group_value, "beta", "SORTBY count DESC must put the largest group first");
    assert_eq!(
        groups[2].group_value, "gamma",
        "SORTBY count DESC must put the smallest group last"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aggregate_filter_eq_narrows_before_counting_on_live_moon() {
    let url = moon_url();
    if !probe_moon(&url) {
        lunaris_test_harness::strict_skip::note_unavailable(format!(
            "SKIP aggregate_filter_eq_narrows_before_counting_on_live_moon: Moon not reachable at {url}"
        ));
        return;
    }

    let scope_str = format!("d3aggf{}", &ulid::Ulid::new().to_string()[..8]);
    let scope = lunaris_core::Scope::new(&scope_str).expect("scope must be valid");
    let dim = 768_usize;
    let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(dim));

    let storage = Arc::new(
        lunaris_storage_moon::MoonStorage::connect_with_dim(&url, dim)
            .await
            .expect("MoonStorage::connect_with_dim must succeed for live Moon"),
    );

    let group_sizes: &[(&str, usize)] = &[("alpha", 4), ("beta", 6)];
    seed_grouped_chunks(&storage, &scope, group_sizes, dim).await;

    let keyword: Arc<dyn KeywordPort> = Arc::new(NoKeyword);
    let ctx = QueryContext::with_moon(
        Query::text("unused"),
        scope,
        embedder,
        storage.clone() as Arc<dyn StoragePort>,
        keyword,
        storage.clone(),
    );

    let groups = Aggregate::count("chunks", "source")
        .filter(Filter::Eq { field: "source".into(), value: serde_json::json!("alpha") })
        .execute(&ctx)
        .await
        .expect("filtered FT.AGGREGATE must succeed against live Moon");

    assert_eq!(groups.len(), 1, "filter must narrow to the single alpha group, got {groups:?}");
    assert_eq!(groups[0].group_value, "alpha");
    assert_eq!(groups[0].count_as_u64(&AggregateReducer::Count), Some(4));
}

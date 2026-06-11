//! ADD task `hotkeys-observability` — server-side classification + gauge
//! aggregation (contract FROZEN @ v1, 2026-06-11). No live backend: a mock
//! `StoragePort` drives `hotkeys_poller::poll_once` and the assertions read
//! the global Prometheus registry.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::scope::Scope;
use lunaris_core::storage::StoragePort;
use lunaris_core::storage::capabilities::StorageCapabilities;
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, HotKey, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_server::hotkeys_poller::{classify_hot_key, poll_once};

// ---------------------------------------------------------------------------
// classification — §2 "maps every Lunaris key shape and drops the rest"
// ---------------------------------------------------------------------------

#[test]
fn classify_maps_lunaris_shapes_and_drops_the_rest() {
    // KV canonical keys → primitive kind.
    let cases: &[(&[u8], Option<(&str, &str)>)] = &[
        (b"lunaris:acme.a1:fact:01HZZZZZZZZZZZZZZZZZZZZZZZ", Some(("acme.a1", "fact"))),
        (b"lunaris:acme.a1:episode:01HZZZZZZZZZZZZZZZZZZZZZZZ", Some(("acme.a1", "episode"))),
        (b"lunaris:acme.a1:chunk:01H", Some(("acme.a1", "chunk"))),
        (b"lunaris:acme.a1:entity:01H", Some(("acme.a1", "entity"))),
        (b"lunaris:acme.a1:relation:01H", Some(("acme.a1", "relation"))),
        (b"lunaris:acme.a1:community:01H", Some(("acme.a1", "community"))),
        (b"lunaris:acme.a1:doctree:01H", Some(("acme.a1", "doctree"))),
        // Unknown-but-canonical kind segment → the closed catch-all label.
        (b"lunaris:acme.a1:somethingnew:01H", Some(("acme.a1", "kv-other"))),
        // FT doc HASH keys → ft-<kind>.
        (b"lunaris_acme.a1_chunks_idx:00ff00ff", Some(("acme.a1", "ft-chunks"))),
        (b"lunaris_acme.a1_entities_idx:00ff", Some(("acme.a1", "ft-entities"))),
        (b"lunaris_acme.a1_facts_idx:00ff", Some(("acme.a1", "ft-facts"))),
        (b"lunaris_acme.a1_communities_idx:00ff", Some(("acme.a1", "ft-communities"))),
        // Graph key.
        (b"lunaris_acme.a1_graph", Some(("acme.a1", "graph"))),
        // Non-Lunaris tenant on the same Moon → dropped.
        (b"sess:123", None),
        (b"user:{42}:name", None),
        // Scope segment failing Scope::new (label-injection defense) → dropped.
        (b"lunaris:bad/scope:fact:01H", None),
        (b"lunaris::fact:01H", None),
    ];
    for (key, want) in cases {
        let got = classify_hot_key(key);
        let got_view = got.as_ref().map(|(s, k)| (s.as_str(), *k));
        assert_eq!(got_view, *want, "key {:?}", String::from_utf8_lossy(key));
    }
    // Invalid UTF-8 → None, never a panic.
    assert!(classify_hot_key(&[0xff, 0xfe, b':']).is_none());
}

// ---------------------------------------------------------------------------
// poller aggregation — mock storage
// ---------------------------------------------------------------------------

/// Mock whose `hot_keys` reply switches per call: tick 1 returns scopes A+B,
/// tick 2 returns only scope A (so B's series must vanish via
/// reset-before-apply). A `NotSupported` mode covers the unsupported arm.
struct HotKeysMock {
    calls: AtomicUsize,
    supported: bool,
}

#[async_trait]
impl StoragePort for HotKeysMock {
    async fn atomic_write(&self, _: &Scope, _: &[WriteOp]) -> Result<Lsn, StorageError> {
        unreachable!("not exercised")
    }
    #[allow(clippy::too_many_arguments)]
    async fn vector_search(
        &self,
        _: &Scope,
        _: &str,
        _: &[f32],
        _: usize,
        _: Option<&Filter>,
        _: Option<Hlc>,
        _: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        unreachable!("not exercised")
    }
    async fn graph_traverse(
        &self,
        _: &Scope,
        _: &CypherQuery,
        _: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        unreachable!("not exercised")
    }
    async fn scan_range(
        &self,
        _: &Scope,
        _: &[u8],
        _: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        unreachable!("not exercised")
    }
    async fn read_as_of(
        &self,
        _: &Scope,
        _: &[u8],
        _: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        unreachable!("not exercised")
    }
    async fn publish(&self, _: &Scope, _: &str, _: u16, _: Bytes) -> Result<u64, StorageError> {
        unreachable!("not exercised")
    }
    async fn subscribe(
        &self,
        _: &Scope,
        _: &str,
        _: &str,
        _: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        unreachable!("not exercised")
    }
    fn capabilities(&self) -> StorageCapabilities {
        unreachable!("not exercised")
    }

    async fn hot_keys(&self, _count: usize) -> Result<Vec<HotKey>, StorageError> {
        if !self.supported {
            return Err(StorageError::NotSupported("hot_keys_unsupported: mock"));
        }
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut out = vec![
            // Scope A: two fact keys (must SUM to 30) + one FT chunk key.
            HotKey { key: b"lunaris:scope-a:fact:01H1".to_vec(), sampled_count: 10 },
            HotKey { key: b"lunaris:scope-a:fact:01H2".to_vec(), sampled_count: 20 },
            HotKey { key: b"lunaris_scope-a_chunks_idx:00ff".to_vec(), sampled_count: 7 },
            // Non-Lunaris key — must be dropped, never labeled.
            HotKey { key: b"sess:123".to_vec(), sampled_count: 999 },
        ];
        if call == 0 {
            out.push(HotKey { key: b"lunaris:scope-b:chunk:01H3".to_vec(), sampled_count: 5 });
        }
        Ok(out)
    }
}

/// Read one series value from the global registry; None if the series is gone.
fn gauge_value(scope: &str, kind: &str) -> Option<i64> {
    for family in prometheus::gather() {
        if family.name() != "lunaris_hotkey_samples" {
            continue;
        }
        for m in family.get_metric() {
            let labels = m.get_label();
            let s = labels.iter().find(|l| l.name() == "scope").map(|l| l.value());
            let k = labels.iter().find(|l| l.name() == "kind").map(|l| l.value());
            if s == Some(scope) && k == Some(kind) {
                return Some(m.get_gauge().value() as i64);
            }
        }
    }
    None
}

/// §2 — aggregation, stale-series clearing, and the NotSupported arm in ONE
/// sequential test (the global Prometheus registry is shared process-wide;
/// parallel tests over the same gauge would race the reset).
#[tokio::test]
async fn poll_once_aggregates_clears_stale_and_survives_not_supported() {
    let mock = Arc::new(HotKeysMock { calls: AtomicUsize::new(0), supported: true });
    let storage: Arc<dyn StoragePort> = mock;

    // Tick 1 — both scopes present, sums per (scope, kind), dropped key absent.
    poll_once(&storage).await;
    assert_eq!(gauge_value("scope-a", "fact"), Some(30), "two fact keys must SUM");
    assert_eq!(gauge_value("scope-a", "ft-chunks"), Some(7));
    assert_eq!(gauge_value("scope-b", "chunk"), Some(5));
    assert!(gauge_value("sess", "kv-other").is_none(), "non-Lunaris keys never become labels");

    // Tick 2 — scope B fell out of the sketch; its series must be GONE.
    poll_once(&storage).await;
    assert_eq!(gauge_value("scope-a", "fact"), Some(30));
    assert_eq!(
        gauge_value("scope-b", "chunk"),
        None,
        "stale series must be cleared by reset-before-apply"
    );

    // NotSupported backend — poll_once must not panic and must NOT touch the
    // gauge ("warn once then quiet": reset only runs when a reply arrives,
    // so a never-supported deployment trivially has zero series and a
    // hypothetical mid-run flip never wipes the last good reading).
    let unsupported: Arc<dyn StoragePort> =
        Arc::new(HotKeysMock { calls: AtomicUsize::new(0), supported: false });
    poll_once(&unsupported).await;
    assert_eq!(gauge_value("scope-a", "fact"), Some(30), "unsupported tick leaves gauge untouched");
}

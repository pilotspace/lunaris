//! Red suite for ADD task `read-api-pagination` (contract v1, FROZEN 2026-06-16).
//!
//! Exercises `lunaris_core::storage::scan_page` — the typed, scope-bound, forward
//! paginated read over `StoragePort::scan_range`. One test per §2 scenario.
//!
//! Until the helper exists these imports are unresolved, so the suite is RED for
//! the right reason (missing implementation), not a wrong-path / harness fault.

use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use ulid::Ulid;

use lunaris_core::keyspace::{fact_key, fact_prefix};
use lunaris_core::*;

// The symbols under test (frozen §3). Unresolved until BUILD lands them.
use lunaris_core::{ListError, MAX_PAGE, Page, scan_page};

// ── Fake StoragePort ────────────────────────────────────────────────────────
// Returns scripted (key,value) pairs from `scan_range`, in whatever order they
// were handed in (so a passing ordering test proves the HELPER sorts). Records
// the `as_of` + `prefix` it was called with and whether it was called at all.

struct FakePort {
    rows: Vec<(Bytes, Bytes)>,
    fail_after: Option<usize>,
    scan_called: AtomicBool,
    observed_as_of: Mutex<Option<Option<Hlc>>>,
    observed_prefix: Mutex<Option<Vec<u8>>>,
}

impl FakePort {
    fn new(rows: Vec<(Bytes, Bytes)>) -> Self {
        Self {
            rows,
            fail_after: None,
            scan_called: AtomicBool::new(false),
            observed_as_of: Mutex::new(None),
            observed_prefix: Mutex::new(None),
        }
    }
    /// Yield `n` Ok rows then an `Err` mid-stream.
    fn with_fail_after(rows: Vec<(Bytes, Bytes)>, n: usize) -> Self {
        let mut p = Self::new(rows);
        p.fail_after = Some(n);
        p
    }
    fn was_scanned(&self) -> bool {
        self.scan_called.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl StoragePort for FakePort {
    async fn scan_range(
        &self,
        _scope: &Scope,
        prefix: &[u8],
        as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        self.scan_called.store(true, Ordering::SeqCst);
        *self.observed_as_of.lock() = Some(as_of);
        *self.observed_prefix.lock() = Some(prefix.to_vec());

        let rows = self.rows.clone();
        let items: Vec<Result<(Bytes, Bytes), StorageError>> = match self.fail_after {
            None => rows.into_iter().map(Ok).collect(),
            Some(n) => rows
                .into_iter()
                .take(n)
                .map(Ok)
                .chain(std::iter::once(Err(StorageError::NotSupported("mid-stream fault"))))
                .collect(),
        };
        Ok(stream::iter(items).boxed())
    }

    // ── Unused by scan_page; never reached. ──
    async fn atomic_write(&self, _: &Scope, _: &[WriteOp]) -> Result<Lsn, StorageError> {
        unreachable!("scan_page must not write")
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
        unreachable!("scan_page must not vector_search")
    }
    async fn graph_traverse(
        &self,
        _: &Scope,
        _: &CypherQuery,
        _: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        unreachable!("scan_page must not graph_traverse")
    }
    async fn read_as_of(
        &self,
        _: &Scope,
        _: &[u8],
        _: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        unreachable!("scan_page must not read_as_of")
    }
    async fn publish(&self, _: &Scope, _: &str, _: u16, _: Bytes) -> Result<u64, StorageError> {
        unreachable!("scan_page must not publish")
    }
    async fn subscribe(
        &self,
        _: &Scope,
        _: &str,
        _: &str,
        _: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        unreachable!("scan_page must not subscribe")
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: true,
            graph_native: true,
            rerank_native: false,
            queue_native: true,
            max_vector_dim: 768,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

// ── Fixtures ──────────────────────────────────────────────────────────────

fn scope(s: &str) -> Scope {
    Scope::new(s).unwrap()
}

/// Monotonic ULID: increasing `i` => strictly increasing ULID (sorts by time).
fn uid(i: u64) -> Ulid {
    Ulid::from_parts(1_700_000_000_000 + i, i as u128)
}

fn fact(sc: &Scope, id: Ulid, text: &str) -> Fact {
    let clock = HlcClock::new(0);
    Fact {
        id,
        scope: sc.clone(),
        subject: id,
        predicate: "rel".into(),
        object: id,
        fact_text: text.into(),
        embedding: None,
        bt: BiTemporal::now(&clock),
        confidence: 1.0,
        provenance: vec![],
        activation: 0.0,
    }
}

fn rows_for(sc: &Scope, facts: &[Fact]) -> Vec<(Bytes, Bytes)> {
    facts
        .iter()
        .map(|f| (Bytes::from(fact_key(sc, f.id)), Bytes::from(serde_json::to_vec(f).unwrap())))
        .collect()
}

/// `n` facts with ULIDs u1<…<un, returned to the fake in SHUFFLED order.
fn shuffled_facts(sc: &Scope, n: u64) -> Vec<Fact> {
    let mut fs: Vec<Fact> = (1..=n).map(|i| fact(sc, uid(i), &format!("fact {i}"))).collect();
    fs.reverse(); // hand the fake a non-sorted order on purpose
    fs
}

// ── Musts ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_typed_page_ulid_order() {
    let sc = scope("acme.agent-1");
    let port = FakePort::new(rows_for(&sc, &shuffled_facts(&sc, 7)));
    let page: Page<Fact> =
        scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), None, 3, None).await.unwrap();

    let ids: Vec<Ulid> = page.items.iter().map(|f| f.id).collect();
    assert_eq!(ids, vec![uid(1), uid(2), uid(3)], "first 3 in ULID order");
    assert!(page.next_cursor.is_some(), "more remain => cursor present");
}

#[tokio::test]
async fn test_last_page_no_cursor() {
    let sc = scope("acme.agent-1");
    let port = FakePort::new(rows_for(&sc, &shuffled_facts(&sc, 3)));
    let page: Page<Fact> =
        scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), None, 10, None).await.unwrap();

    assert_eq!(page.items.len(), 3);
    assert!(page.next_cursor.is_none(), "page is the last => no cursor");
}

#[tokio::test]
async fn test_full_walk_each_once() {
    let sc = scope("acme.agent-1");
    let port = FakePort::new(rows_for(&sc, &shuffled_facts(&sc, 7)));

    let mut seen: Vec<Ulid> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page: Page<Fact> =
            scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), cursor.as_deref(), 3, None)
                .await
                .unwrap();
        seen.extend(page.items.iter().map(|f| f.id));
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    let expect: Vec<Ulid> = (1..=7).map(uid).collect();
    assert_eq!(seen, expect, "every item exactly once, in order, none skipped/repeated");
}

#[tokio::test]
async fn test_cursor_resumes_after_tail() {
    let sc = scope("acme.agent-1");
    let port = FakePort::new(rows_for(&sc, &shuffled_facts(&sc, 7)));

    let page1: Page<Fact> =
        scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), None, 3, None).await.unwrap();
    let cur = page1.next_cursor.clone().expect("cursor after page 1");

    let page2: Page<Fact> =
        scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), Some(&cur), 3, None).await.unwrap();
    let ids: Vec<Ulid> = page2.items.iter().map(|f| f.id).collect();
    assert_eq!(ids, vec![uid(4), uid(5), uid(6)], "resumes strictly after u3");
    assert!(!ids.contains(&uid(1)) && !ids.contains(&uid(3)), "page-1 items do not reappear");
}

#[tokio::test]
async fn test_empty_cursor_from_first() {
    let sc = scope("acme.agent-1");
    let port = FakePort::new(rows_for(&sc, &shuffled_facts(&sc, 5)));
    let page: Page<Fact> =
        scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), None, 2, None).await.unwrap();
    assert_eq!(page.items[0].id, uid(1), "empty cursor starts at first key");
}

#[tokio::test]
async fn test_opaque_cursor_roundtrip() {
    let sc = scope("acme.agent-1");
    let port = FakePort::new(rows_for(&sc, &shuffled_facts(&sc, 7)));

    let p1: Page<Fact> =
        scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), None, 2, None).await.unwrap();
    let token = p1.next_cursor.clone().unwrap(); // caller treats as opaque
    let p2: Page<Fact> =
        scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), Some(&token), 2, None).await.unwrap();

    assert_eq!(p1.items.last().unwrap().id, uid(2));
    assert_eq!(p2.items.first().unwrap().id, uid(3), "page 2 resumes after page-1 tail");
}

#[tokio::test]
async fn test_full_primitive_not_truncated() {
    let sc = scope("acme.agent-1");
    let mut f = fact(&sc, uid(1), &"x".repeat(4096));
    f.embedding = Some(vec![0.1, 0.2, 0.3]);
    f.provenance = vec![uid(50), uid(51)];

    let port = FakePort::new(rows_for(&sc, std::slice::from_ref(&f)));
    let page: Page<Fact> =
        scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), None, 10, None).await.unwrap();

    assert_eq!(page.items.len(), 1);
    // W3 embedding double-store fix: `embedding` is skip_serializing (the
    // vector lives only in the FT index / vector column), so it is
    // intentionally absent after a storage roundtrip. Everything else must
    // survive untruncated.
    assert_eq!(
        page.items[0].embedding, None,
        "embedding never round-trips through KV (skip_serializing)"
    );
    let mut expect = f.clone();
    expect.embedding = None;
    assert_eq!(page.items[0], expect, "returned Fact equals stored Fact across all other fields");
}

#[tokio::test]
async fn test_scope_isolation() {
    let a = scope("acme.agent-A");
    let port = FakePort::new(rows_for(&a, &shuffled_facts(&a, 4)));

    let page: Page<Fact> =
        scan_page::<Fact>(&port, &a, &fact_prefix(&a), None, 10, None).await.unwrap();

    assert!(page.items.iter().all(|f| f.scope == a), "every item belongs to scope A");
    assert_eq!(
        port.observed_prefix.lock().clone(),
        Some(fact_prefix(&a)),
        "helper forwards the scope-derived prefix unchanged (backend filters keys by it)"
    );
}

#[tokio::test]
async fn test_as_of_passthrough() {
    let sc = scope("acme.agent-1");
    let port = FakePort::new(rows_for(&sc, &shuffled_facts(&sc, 2)));
    let when = HlcClock::new(0).tick();

    let _ = scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), None, 10, Some(when)).await.unwrap();

    assert_eq!(
        port.observed_as_of.lock().clone(),
        Some(Some(when)),
        "the as_of is passed through to scan_range untouched"
    );
}

// ── Rejects ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_zero_limit_rejected() {
    let sc = scope("acme.agent-1");
    let port = FakePort::new(rows_for(&sc, &shuffled_facts(&sc, 3)));
    let err = scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), None, 0, None).await.unwrap_err();

    assert_eq!(err.code(), "invalid_limit");
    assert!(!port.was_scanned(), "validation precedes any I/O");
}

#[tokio::test]
async fn test_over_cap_rejected() {
    let sc = scope("acme.agent-1");
    let port = FakePort::new(rows_for(&sc, &shuffled_facts(&sc, 3)));
    let err = scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), None, MAX_PAGE + 1, None)
        .await
        .unwrap_err();

    assert_eq!(err.code(), "limit_too_large");
    assert!(!port.was_scanned(), "validation precedes any I/O");
}

#[tokio::test]
async fn test_malformed_cursor_rejected() {
    let sc = scope("acme.agent-1");
    let port = FakePort::new(rows_for(&sc, &shuffled_facts(&sc, 3)));
    let err = scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), Some("not-a-real-token"), 3, None)
        .await
        .unwrap_err();

    assert_eq!(err.code(), "invalid_cursor");
    assert!(!port.was_scanned(), "cursor validated before any I/O");
}

#[tokio::test]
async fn test_corrupt_row_stops_page() {
    let sc = scope("acme.agent-1");
    // A valid key under the prefix, but a value that is not JSON for Fact.
    let bad = vec![(Bytes::from(fact_key(&sc, uid(1))), Bytes::from_static(b"{ not json"))];
    let port = FakePort::new(bad);

    let err = scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), None, 10, None).await.unwrap_err();
    assert_eq!(err.code(), "corrupt_row", "an undeserializable page value stops the page");
}

#[tokio::test]
async fn test_midstream_error_propagates() {
    let sc = scope("acme.agent-1");
    // Two Ok rows then an Err.
    let port = FakePort::with_fail_after(rows_for(&sc, &shuffled_facts(&sc, 5)), 2);

    let result = scan_page::<Fact>(&port, &sc, &fact_prefix(&sc), None, 10, None).await;
    assert!(
        matches!(result, Err(ListError::Storage(_))),
        "a mid-stream scan error propagates as Storage(_), never a short page sold as complete"
    );
}

//! F8 — a `.tree(..)` recall against a scope with no RAPTOR tree must SAY SO.
//!
//! RAPTOR became opt-in, so `.tree(..)` over a corpus ingested without it finds
//! no communities index and returns an empty result. That is the right value —
//! F1 established that a missing index is "nothing written yet", not an error —
//! but it was announced at `debug!`, which nothing runs at. A user who opted
//! into tree retrieval and silently got flat results had no way to learn why.
//!
//! Silent-empty is the failure mode this codebase keeps digging out (F16's
//! `mode=graph`, F22's zero vectors, the RAPTOR parity suite that compared two
//! empty trees in F6). The fix is not to fail — it is to be loud.
//!
//! The pair matters as much as the assertion. A warning on EVERY empty tree
//! recall would be worse than none: it would train the reader to ignore it. So
//! the second test pins that a present-but-empty index stays quiet.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    Embedder, Hlc, Scope, StorageCapabilities, StorageError, StoragePort, StubEmbedder,
};
use lunaris_retrieve::{Query, QueryContext, Retriever, Tree};
use parking_lot::Mutex;

/// `vector_search` either reports the index as absent, or answers empty.
struct TreeStorage {
    index_absent: bool,
}

#[async_trait]
impl StoragePort for TreeStorage {
    async fn atomic_write(&self, _scope: &Scope, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        Ok(Lsn::ZERO)
    }
    async fn vector_search(
        &self,
        _scope: &Scope,
        index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        if self.index_absent {
            // The exact wording Moon uses for a lazily-created index that was
            // never written — see `missing_index::is_index_absent`.
            return Err(StorageError::Backend(format!("unknown index {index}")));
        }
        Ok(Vec::new())
    }
    async fn graph_traverse(
        &self,
        _scope: &Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        Err(StorageError::NotSupported("TreeStorage::graph_traverse"))
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
        Err(StorageError::NotSupported("TreeStorage::publish"))
    }
    async fn subscribe(
        &self,
        _scope: &Scope,
        _group: &str,
        _topic: &str,
        _partition: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("TreeStorage::subscribe"))
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
impl KeywordPort for TreeStorage {
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

/// A `MakeWriter` that collects everything the subscriber emits.
#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl CapturedLog {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock()).into_owned()
    }
}

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = CapturedLog;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Run one `Tree` retrieval with WARN-and-above captured.
fn tree_recall_capturing_warnings(index_absent: bool) -> (Vec<lunaris_retrieve::RawHit>, String) {
    let log = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(log.clone())
        .finish();

    let hits = tracing::subscriber::with_default(subscriber, || {
        let storage: Arc<dyn StoragePort> = Arc::new(TreeStorage { index_absent });
        let keyword: Arc<dyn KeywordPort> = Arc::new(TreeStorage { index_absent });
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new(768));
        let ctx = QueryContext::new(
            Query::text("what happened"),
            Scope::dev(),
            embedder,
            storage,
            keyword,
        );
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async { Tree::new("communities", 5).retrieve(&ctx).await })
    })
    .expect("a missing tree index is empty, never an error (F1)");

    (hits, log.text())
}

/// The finding: opting into `.tree(..)` without a tree must be audible.
#[test]
fn a_missing_tree_index_warns_instead_of_answering_empty_in_silence() {
    let (hits, log) = tree_recall_capturing_warnings(true);

    assert!(hits.is_empty(), "a missing index still yields no hits — F1 unchanged");
    assert!(
        log.contains("WARN"),
        "a missing tree index must be announced at WARN; nothing runs at debug. log={log:?}"
    );
    // The message has to be actionable: a reader who sees it should learn that
    // RAPTOR is opt-in and that existing data needs re-ingesting. Asserting on
    // the CAUSE, not on a phrase, so rewording the sentence does not red this.
    for needle in ["raptor", "re-ingest"] {
        assert!(
            log.to_lowercase().contains(needle),
            "warning must name {needle:?} so the reader knows what to do. log={log:?}"
        );
    }
}

/// Vacuity floor, and the reason the test above is safe to add: a tree that
/// EXISTS and simply matched nothing must stay silent. A warning on every empty
/// tree recall would be noise, and noise is how a real warning gets ignored.
#[test]
fn an_existing_but_empty_tree_index_stays_quiet() {
    let (hits, log) = tree_recall_capturing_warnings(false);

    assert!(hits.is_empty(), "no communities matched, so no hits");
    assert!(
        !log.contains("WARN"),
        "an empty-but-present tree index is not a misconfiguration and must not warn. log={log:?}"
    );
}

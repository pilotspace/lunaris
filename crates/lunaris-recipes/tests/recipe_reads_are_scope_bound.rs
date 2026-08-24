//! Every recipe read path must carry the scope the recipe was constructed
//! with (W4.17).
//!
//! ## Why this test is at the port boundary, not at the result set
//!
//! The sibling test `timeline_prefix_scoped_recall.rs` pins the *source
//! prefix* by asserting on returned rows. That shape cannot pin the *scope*:
//! a stub `StoragePort` has one row store, so a read issued against the wrong
//! partition still comes back with the right rows and the assertion holds.
//! Worse, the empty case passes too — see the exclusion trap that let W4.12's
//! first green through. So this test records the `&Scope` argument each port
//! method actually receives and asserts on that. The discriminator is at the
//! point of measurement.
//!
//! ## What it caught
//!
//! Five of the six recipe read paths never put the scope on the query:
//! `MessageStream::recall`, `DocumentCorpus::search`, `TemporalQuery::execute`,
//! `SlackArchive*::recall` and `MeetingNotes*::recall` all built
//! `lunaris.recall()` with no `.with_scope`, which defaults to `Scope::dev()`.
//! Writes went to `self.scope` the whole time. A recipe therefore wrote into
//! its own partition and recalled across **every** partition — the F30 defect
//! one layer up.
//!
//! It stayed invisible because every SDK recipe binding constructed at
//! `Scope::dev()` too (10 call sites per SDK in `generated.rs`), so the two
//! halves agreed by accident and no caller could tell them apart. W4.17 fixed
//! the bindings first, which is what exposed this.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream, StreamExt};
use lunaris::Lunaris;
use lunaris_core::storage::keyword::{KeywordHit, KeywordPort};
use lunaris_core::storage::types::{
    CypherQuery, Filter, GraphResult, Lsn, QueueMsg, Row, VectorHit, WriteOp,
};
use lunaris_core::{
    Embedder, Hlc, HlcClock, LunarisError, Scope, StorageCapabilities, StorageError, StoragePort,
};
use parking_lot::Mutex;

const DIM: usize = 768;

struct StubEmbedder;

#[async_trait]
impl Embedder for StubEmbedder {
    fn dim(&self) -> usize {
        DIM
    }
    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        Ok(inputs.iter().map(|_| vec![0.1f32; DIM]).collect())
    }
}

/// Records the `Scope` every READ arrives with. Writes are recorded
/// separately: a recipe whose write and read disagree is the exact defect,
/// so conflating the two would hide it.
#[derive(Default)]
struct ScopeRecordingStorage {
    reads: Mutex<Vec<String>>,
    writes: Mutex<Vec<String>>,
}

impl ScopeRecordingStorage {
    fn take_reads(&self) -> Vec<String> {
        std::mem::take(&mut *self.reads.lock())
    }
    fn take_writes(&self) -> Vec<String> {
        std::mem::take(&mut *self.writes.lock())
    }
}

#[async_trait]
impl StoragePort for ScopeRecordingStorage {
    async fn atomic_write(&self, scope: &Scope, _ops: &[WriteOp]) -> Result<Lsn, StorageError> {
        self.writes.lock().push(scope.as_str().to_string());
        Ok(Lsn { wall_ms: 1, counter: 1 })
    }
    async fn vector_search(
        &self,
        scope: &Scope,
        _index: &str,
        _query: &[f32],
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
        _rerank: bool,
    ) -> Result<Vec<VectorHit>, StorageError> {
        self.reads.lock().push(scope.as_str().to_string());
        Ok(Vec::new())
    }
    async fn graph_traverse(
        &self,
        scope: &Scope,
        _q: &CypherQuery,
        _as_of: Option<Hlc>,
    ) -> Result<GraphResult, StorageError> {
        self.reads.lock().push(scope.as_str().to_string());
        Ok(GraphResult::default())
    }
    async fn scan_range(
        &self,
        scope: &Scope,
        _prefix: &[u8],
        _as_of: Option<Hlc>,
    ) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError> {
        self.reads.lock().push(scope.as_str().to_string());
        Ok(stream::iter(Vec::<Result<(Bytes, Bytes), StorageError>>::new()).boxed())
    }
    async fn read_as_of(
        &self,
        scope: &Scope,
        _key: &[u8],
        _as_of: Hlc,
    ) -> Result<Option<Row<Bytes>>, StorageError> {
        self.reads.lock().push(scope.as_str().to_string());
        Ok(None)
    }
    async fn publish(
        &self,
        _s: &Scope,
        _t: &str,
        _p: u16,
        _payload: Bytes,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }
    async fn subscribe(
        &self,
        _s: &Scope,
        _g: &str,
        _t: &str,
        _p: u16,
    ) -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError> {
        Err(StorageError::NotSupported("ScopeRecordingStorage::subscribe"))
    }
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities {
            bi_temporal_native: false,
            graph_native: false,
            rerank_native: false,
            queue_native: false,
            max_vector_dim: DIM as u32,
            native_rrf: false,
            max_scopes_recommended: 0,
            cypher_dialect: lunaris_core::CypherDialect::Legacy,
            graph_decay_native: false,
            graph_navigate_native: false,
        }
    }
}

#[async_trait]
impl KeywordPort for ScopeRecordingStorage {
    async fn keyword_search(
        &self,
        scope: &Scope,
        _index: &str,
        _query: &str,
        _k: usize,
        _filter: Option<&Filter>,
        _as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError> {
        self.reads.lock().push(scope.as_str().to_string());
        Ok(Vec::new())
    }
}

fn make_handle(rec: Arc<ScopeRecordingStorage>) -> Arc<Lunaris> {
    Arc::new(Lunaris::with_parts_keyword(
        rec.clone() as Arc<dyn StoragePort>,
        rec as Arc<dyn KeywordPort>,
        Arc::new(StubEmbedder) as Arc<dyn Embedder>,
        HlcClock::new(0),
    ))
}

/// The recipe's own partition. Deliberately NOT `Scope::dev()`: that is the
/// value an unbound `lunaris.recall()` falls back to, so a test run at
/// `Scope::dev()` cannot distinguish "bound correctly" from "not bound at
/// all" — every assertion below would pass on the pre-W4.17 code.
fn scope() -> Scope {
    Scope::new("w417-recipe-partition").expect("valid scope")
}

fn assert_reads_bound(rec: &ScopeRecordingStorage, what: &str) {
    let reads = rec.take_reads();
    assert!(
        !reads.is_empty(),
        "{what}: the read path issued NO port call at all, so this assertion \
         proves nothing. Either the recipe short-circuited or the stub is not \
         on the path being exercised."
    );
    let distinct: HashSet<&str> = reads.iter().map(String::as_str).collect();
    assert_eq!(
        distinct,
        HashSet::from(["w417-recipe-partition"]),
        "{what}: a read reached the storage port under the wrong partition. \
         Expected every one of the {} call(s) at `w417-recipe-partition`; saw \
         {distinct:?}. `_dev_` here means the recipe built `lunaris.recall()` \
         without `.with_scope(self.scope.clone())` and is reading across every \
         tenant in the store.",
        reads.len()
    );
}

fn assert_writes_bound(rec: &ScopeRecordingStorage, what: &str) {
    let writes = rec.take_writes();
    assert!(!writes.is_empty(), "{what}: the ingest issued no write at all");
    let distinct: HashSet<&str> = writes.iter().map(String::as_str).collect();
    assert_eq!(
        distinct,
        HashSet::from(["w417-recipe-partition"]),
        "{what}: a write landed in the wrong partition; saw {distinct:?}"
    );
}

fn chunk() -> (String, serde_json::Map<String, serde_json::Value>) {
    ("the quick brown fox jumps".to_string(), serde_json::Map::new())
}

#[tokio::test]
async fn message_stream_recall_carries_its_scope() {
    let rec = Arc::new(ScopeRecordingStorage::default());
    let ms = lunaris_recipes::MessageStream::new(make_handle(rec.clone()), scope(), "chat:u/");

    ms.ingest("hello there", "thread-1", "u").await.expect("ingest");
    assert_writes_bound(&rec, "MessageStream::ingest");

    ms.recall("hello").await.expect("recall");
    assert_reads_bound(&rec, "MessageStream::recall");
}

#[tokio::test]
async fn document_corpus_search_carries_its_scope() {
    let rec = Arc::new(ScopeRecordingStorage::default());
    let handle = make_handle(rec.clone());
    let kb = lunaris_recipes::DocumentCorpus::new(handle.clone(), scope(), "kb:docs/");

    kb.ingest(vec![chunk()]).await.expect("ingest");
    assert_writes_bound(&rec, "DocumentCorpus::ingest");

    lunaris_recipes::DocumentCorpus::new(handle, scope(), "kb:docs/")
        .search("fox")
        .await
        .expect("search");
    assert_reads_bound(&rec, "DocumentCorpus::search");
}

#[tokio::test]
async fn temporal_query_execute_carries_its_scope() {
    let rec = Arc::new(ScopeRecordingStorage::default());
    let handle = make_handle(rec.clone());

    lunaris_recipes::TemporalQuery::<lunaris_recipes::Documents>::new(handle, scope())
        .before(Hlc { wall_ms: 1_736_985_600_000, counter: 0, node_id: 0 })
        .execute("fox")
        .await
        .expect("execute");
    assert_reads_bound(&rec, "TemporalQuery::execute");
}

#[tokio::test]
async fn slack_archive_recall_carries_its_scope_on_both_surfaces() {
    let rec = Arc::new(ScopeRecordingStorage::default());
    let slack =
        lunaris_recipes::conversational::SlackArchive::new(make_handle(rec.clone()), scope());

    slack.ingest_channel("C-general", "alice", "shipping today").await.expect("ingest");
    assert_writes_bound(&rec, "SlackArchive::ingest_channel");

    slack.recall("shipping").await.expect("recall");
    assert_reads_bound(&rec, "SlackArchive::recall");

    // The narrowed query builder is a SEPARATE struct with its own recall.
    // Fixing only the parent would leave this one reading every partition —
    // it is the copy a caller actually uses for channel/user filters.
    slack.channel("C-general").with_user("alice").recall("shipping").await.expect("query recall");
    assert_reads_bound(&rec, "SlackArchiveQuery::recall");
}

#[tokio::test]
async fn meeting_notes_recall_carries_its_scope_on_both_surfaces() {
    let rec = Arc::new(ScopeRecordingStorage::default());
    let mtg =
        lunaris_recipes::conversational::MeetingNotesMemory::new(make_handle(rec.clone()), scope());

    mtg.note("Q2 planning", "discussed roadmap and staffing").await.expect("note");
    assert_writes_bound(&rec, "MeetingNotesMemory::note");

    mtg.recall("staffing").await.expect("recall");
    assert_reads_bound(&rec, "MeetingNotesMemory::recall");

    mtg.attendees(vec!["alice".to_string()]).recall("staffing").await.expect("query recall");
    assert_reads_bound(&rec, "MeetingNotesQuery::recall");
}

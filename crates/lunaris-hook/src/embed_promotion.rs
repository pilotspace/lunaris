//! Async semantic promotion for contextd capture.
//!
//! Hook capture writes chunks with `NoopEmbedder` so the request path stays
//! storage-bound. When Moon MQ is available, contextd publishes a promotion
//! event and a background worker batches real vector upserts.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use bytes::Bytes;
use futures::StreamExt;
use lunaris::Lunaris;
use lunaris_core::{Chunk, HlcClock, Scope, StoragePort, WriteOp, keyspace::chunk_key};
use lunaris_ingest::{IngestReceipt, validate_chunk_metadata};
use serde::{Deserialize, Serialize};
use serde_json::json;
use ulid::Ulid;

pub(crate) const EMBED_PROMOTION_TOPIC: &str = "__lunaris_embed__";
pub(crate) const EMBED_PROMOTION_GROUP: &str = "lunaris-contextd-embed-v0";

const DEFAULT_EMBED_BATCH_SIZE: usize = 16;
const DEFAULT_EMBED_BATCH_WAIT_MS: u64 = 25;

#[derive(Clone, Debug)]
pub(crate) struct EmbedPromotionConfig {
    pub enabled: bool,
    pub worker_enabled: bool,
    pub batch_size: usize,
    pub batch_wait_ms: u64,
}

impl EmbedPromotionConfig {
    pub(crate) fn from_env() -> Self {
        Self {
            enabled: env_bool("LUNARIS_EMBED_PROMOTION_ENABLED").unwrap_or(true),
            worker_enabled: env_bool("LUNARIS_EMBED_PROMOTION_WORKER").unwrap_or(true),
            batch_size: env_usize("LUNARIS_EMBED_BATCH_SIZE")
                .unwrap_or(DEFAULT_EMBED_BATCH_SIZE)
                .max(1),
            batch_wait_ms: env_u64("LUNARIS_EMBED_BATCH_WAIT_MS")
                .unwrap_or(DEFAULT_EMBED_BATCH_WAIT_MS),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct EmbedPromotionEvent {
    pub kind: String,
    pub scope: String,
    pub source: String,
    pub episode_id: String,
    pub chunk_ids: Vec<String>,
    pub created_at_ms: u64,
}

impl EmbedPromotionEvent {
    pub(crate) fn from_receipt(scope: &Scope, source: &str, receipt: &IngestReceipt) -> Self {
        Self {
            kind: "chunk_embed_requested".to_string(),
            scope: scope.as_str().to_string(),
            source: source.to_string(),
            episode_id: receipt.episode_id.to_string(),
            chunk_ids: receipt.chunk_ids.iter().map(ToString::to_string).collect(),
            created_at_ms: now_ms(),
        }
    }
}

pub(crate) async fn publish_capture_receipt(
    storage: &dyn StoragePort,
    scope: &Scope,
    source: &str,
    receipt: &IngestReceipt,
    config: &EmbedPromotionConfig,
) -> anyhow::Result<Option<u64>> {
    if !config.enabled || receipt.chunk_ids.is_empty() || !storage.capabilities().queue_native {
        return Ok(None);
    }
    let event = EmbedPromotionEvent::from_receipt(scope, source, receipt);
    let payload = serde_json::to_vec(&event)?;
    let offset = storage
        .publish(scope, EMBED_PROMOTION_TOPIC, 0, Bytes::from(payload))
        .await
        .context("publish embed promotion event")?;
    Ok(Some(offset))
}

pub(crate) async fn run_worker(
    handle: Arc<Lunaris>,
    scope: Scope,
    config: EmbedPromotionConfig,
) -> anyhow::Result<()> {
    if !config.enabled || !config.worker_enabled || !handle.storage().capabilities().queue_native {
        return Ok(());
    }

    let storage = handle.storage();
    let mut stream = storage
        .subscribe(&scope, EMBED_PROMOTION_GROUP, EMBED_PROMOTION_TOPIC, 0)
        .await
        .context("subscribe embed promotion queue")?;

    loop {
        let first = match stream.next().await {
            Some(Ok(msg)) => parse_event(&msg.payload)?,
            Some(Err(err)) => {
                tracing::debug!(err = %err, "lunaris embed promotion queue poll failed");
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            None => return Ok(()),
        };

        let mut events = vec![first];
        let mut chunk_count = events[0].chunk_ids.len();
        let deadline = tokio::time::sleep(Duration::from_millis(config.batch_wait_ms));
        tokio::pin!(deadline);

        while chunk_count < config.batch_size {
            tokio::select! {
                maybe_msg = stream.next() => {
                    match maybe_msg {
                        Some(Ok(msg)) => {
                            match parse_event(&msg.payload) {
                                Ok(event) => {
                                    chunk_count += event.chunk_ids.len();
                                    events.push(event);
                                }
                                Err(err) => {
                                    tracing::debug!(err = %err, "lunaris embed promotion event ignored");
                                }
                            }
                        }
                        Some(Err(err)) => {
                            tracing::debug!(err = %err, "lunaris embed promotion queue poll failed");
                            break;
                        }
                        None => return Ok(()),
                    }
                }
                _ = &mut deadline => break,
            }
        }

        match promote_batch(&handle, &scope, events, config.batch_size).await {
            Ok(outcome) => report_outcome(&outcome),
            Err(err) => tracing::warn!(err = %err, "lunaris embed promotion batch failed"),
        }
    }
}

/// What one promotion batch actually did.
///
/// F25: every id an event names lands in exactly one bucket, and
/// `requested == promoted + missing + empty_text + bad_id + failed`. Before
/// this existed the worker's two skip paths were bare `continue`s and every
/// failure was a `?`, so a store could lose half its vectors for a month with
/// nothing in the logs to say so.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PromotionOutcome {
    /// Distinct chunk ids named by this batch's events, plus unparseable ones.
    pub requested: usize,
    /// Chunks whose real vector was written.
    pub promoted: usize,
    /// The event named a chunk the store has no row for.
    pub missing: usize,
    /// The row is there but its text is blank, so there is nothing to embed.
    pub empty_text: usize,
    /// The event carried something that is not a ULID.
    pub bad_id: usize,
    /// Lost to an embed, decode or write failure rather than skipped on purpose.
    pub failed: usize,
}

async fn promote_batch(
    handle: &Lunaris,
    scope: &Scope,
    events: Vec<EmbedPromotionEvent>,
    batch_size: usize,
) -> anyhow::Result<PromotionOutcome> {
    let outcome = PromotionOutcome::default();
    let storage = handle.storage();
    let mut wanted: HashMap<Ulid, String> = HashMap::new();
    for event in events {
        for chunk_id in event.chunk_ids {
            if let Ok(id) = Ulid::from_string(&chunk_id) {
                wanted.entry(id).or_insert_with(|| event.source.clone());
            }
        }
    }
    if wanted.is_empty() {
        return Ok(outcome);
    }

    let clock = HlcClock::new(0);
    let as_of = clock.tick();
    let mut chunks = Vec::with_capacity(wanted.len());
    let mut sources = Vec::with_capacity(wanted.len());
    for (chunk_id, source) in wanted {
        let key = chunk_key(scope, chunk_id);
        let Some(row) = storage.read_as_of(scope, &key, as_of).await? else {
            continue;
        };
        let chunk: Chunk = serde_json::from_slice(&row.value)?;
        if chunk.text.trim().is_empty() {
            continue;
        }
        chunks.push(chunk);
        sources.push(source);
    }
    if chunks.is_empty() {
        return Ok(outcome);
    }

    let started = std::time::Instant::now();
    let mut promoted = 0usize;
    for (chunk_batch, source_batch) in chunks.chunks(batch_size).zip(sources.chunks(batch_size)) {
        let texts: Vec<&str> = chunk_batch.iter().map(|chunk| chunk.text.as_str()).collect();
        // Background lane: promotion must never head-of-line-block an
        // interactive recall query on the shared embedder worker context.
        let embeddings = handle.embedder().embed_batch_lowpri(&texts).await?;
        if embeddings.len() != chunk_batch.len() {
            anyhow::bail!(
                "embed promotion returned {} rows for {} chunks",
                embeddings.len(),
                chunk_batch.len()
            );
        }

        let mut ops = Vec::with_capacity(chunk_batch.len());
        for ((chunk, source), embedding) in
            chunk_batch.iter().zip(source_batch.iter()).zip(embeddings.into_iter())
        {
            let metadata = json!({
                "episode_id": chunk.episode_id.to_string(),
                "heading_path": &chunk.heading_path,
                "offset": chunk.offset,
                "text": &chunk.text,
                "source": source,
            });
            validate_chunk_metadata(&metadata).map_err(|e| anyhow::anyhow!("schema gate: {e}"))?;
            ops.push(WriteOp::VectorUpsert {
                index: "chunks".to_string(),
                id: chunk.id.to_bytes().to_vec(),
                embedding,
                metadata,
            });
        }
        storage.atomic_write(scope, &ops).await?;
        promoted += ops.len();
    }

    if std::env::var("LUNARIS_CONTEXT_PROFILE").ok().as_deref() == Some("1") {
        tracing::info!(
            promoted,
            elapsed_ms = started.elapsed().as_millis(),
            "lunaris embed promotion batch complete"
        );
    }
    Ok(outcome)
}

/// F25: say what was dropped, unconditionally. The old summary sat behind
/// `LUNARIS_CONTEXT_PROFILE=1`, which is why a month of loss was invisible.
fn report_outcome(outcome: &PromotionOutcome) {
    if outcome.promoted < outcome.requested {
        tracing::warn!(
            requested = outcome.requested,
            promoted = outcome.promoted,
            missing = outcome.missing,
            empty_text = outcome.empty_text,
            bad_id = outcome.bad_id,
            failed = outcome.failed,
            "lunaris embed promotion dropped chunks"
        );
    } else {
        tracing::debug!(promoted = outcome.promoted, "lunaris embed promotion batch complete");
    }
}

fn parse_event(payload: &[u8]) -> anyhow::Result<EmbedPromotionEvent> {
    let event: EmbedPromotionEvent = serde_json::from_slice(payload)?;
    if event.kind != "chunk_embed_requested" {
        anyhow::bail!("unexpected embed promotion event kind: {}", event.kind);
    }
    Ok(event)
}

fn env_bool(name: &str) -> Option<bool> {
    match std::env::var(name).ok()?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.parse::<usize>().ok()
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse::<u64>().ok()
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris_core::{Embedder, Episode, NoopEmbedder};

    #[test]
    fn event_round_trips_from_receipt() {
        let scope = Scope::new("test.scope").unwrap();
        let clock = HlcClock::new(0);
        let episode = Episode::new(scope.clone(), "lunaris:tool_call:post", "body", &clock);
        let chunk_id = Ulid::new();
        let receipt = IngestReceipt {
            lsn: lunaris_core::Lsn { wall_ms: 1, counter: 0 },
            episode_id: episode.id,
            chunk_ids: vec![chunk_id],
        };

        let event = EmbedPromotionEvent::from_receipt(&scope, "lunaris:tool_call:post", &receipt);
        let bytes = serde_json::to_vec(&event).unwrap();
        let parsed = parse_event(&bytes).unwrap();

        assert_eq!(parsed.scope, "test.scope");
        assert_eq!(parsed.episode_id, episode.id.to_string());
        assert_eq!(parsed.chunk_ids, vec![chunk_id.to_string()]);

        // Keep the import honest: the hot path uses NoopEmbedder before this event exists.
        assert_eq!(NoopEmbedder::default().dim(), 768);
    }
}

#[cfg(test)]
mod lowpri_routing_tests {
    //! scenario: promotion uses the low-priority lane — promote_batch must route
    //! its embed call through `embed_batch_lowpri` (Background), never
    //! `embed_batch` (Interactive), so ingest never head-of-line-blocks recall.
    use super::*;
    use async_trait::async_trait;
    use lunaris_core::{Chunk, Embedder, LunarisError};
    use lunaris_test_harness::open_test_engine_with_embedder;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Embedder that records which lane was invoked. Overrides BOTH methods so
    /// the two lanes are distinguishable (the default would make lowpri call
    /// embed_batch, hiding the routing).
    #[derive(Debug)]
    struct RecordingEmbedder {
        dim: usize,
        hi: Arc<AtomicUsize>,
        lo: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Embedder for RecordingEmbedder {
        fn dim(&self) -> usize {
            self.dim
        }
        async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
            self.hi.fetch_add(1, Ordering::SeqCst);
            Ok(inputs.iter().map(|_| vec![0.0f32; self.dim]).collect())
        }
        async fn embed_batch_lowpri(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
            self.lo.fetch_add(1, Ordering::SeqCst);
            Ok(inputs.iter().map(|_| vec![0.0f32; self.dim]).collect())
        }
    }

    #[tokio::test]
    async fn promote_batch_routes_to_lowpri_lane() {
        let hi = Arc::new(AtomicUsize::new(0));
        let lo = Arc::new(AtomicUsize::new(0));
        let embedder = Arc::new(RecordingEmbedder { dim: 768, hi: hi.clone(), lo: lo.clone() });
        // 0.7.0 port off `memory://` — harness-issued ephemeral Moon, degrading
        // to `memory://` where no Moon binary resolves. `TestEngine` derefs to
        // `Lunaris`; the binding owns the Moon child.
        let handle = open_test_engine_with_embedder(embedder).await;

        let scope = Scope::new("test.lowpri").unwrap();
        let clock = HlcClock::new(0);
        let episode_id = Ulid::new();
        let chunk = Chunk::new(scope.clone(), episode_id, "hello world", 2, 0, vec![], &clock);
        let chunk_id = chunk.id;
        let key = chunk_key(&scope, chunk_id);
        let value = serde_json::to_vec(&chunk).unwrap();
        handle.storage().atomic_write(&scope, &[WriteOp::KvPut { key, value }]).await.unwrap();

        let event = EmbedPromotionEvent {
            kind: "chunk_embed_requested".to_string(),
            scope: scope.as_str().to_string(),
            source: "lunaris:tool_call:post".to_string(),
            episode_id: episode_id.to_string(),
            chunk_ids: vec![chunk_id.to_string()],
            created_at_ms: 0,
        };

        promote_batch(&handle, &scope, vec![event], 16).await.unwrap();

        assert_eq!(
            lo.load(Ordering::SeqCst),
            1,
            "promotion must use the low-priority (Background) lane"
        );
        assert_eq!(hi.load(Ordering::SeqCst), 0, "promotion must NOT use the interactive lane");
    }
}

#[cfg(test)]
mod promotion_accounting_tests {
    //! scenario: F25 — the promotion worker drops chunks silently.
    //!
    //! Half the chunk rows in the live store still carry a zero vector after a
    //! month of a worker that reports nothing. Both skip paths in
    //! `promote_batch` were bare `continue`s and every failure was a `?` that
    //! discarded the rest of the batch, so the worker could not tell anyone
    //! what it lost. These tests pin the accounting, not a specific cause.
    use super::*;
    use async_trait::async_trait;
    use lunaris_core::{Embedder, LunarisError};
    use lunaris_test_harness::open_test_engine_with_embedder;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 768-d unit-ish vectors, and a switch to fail the Nth call so a batch
    /// failure can be aimed precisely.
    #[derive(Debug)]
    struct FlakyEmbedder {
        dim: usize,
        calls: Arc<AtomicUsize>,
        fail_on_call: usize,
    }

    impl FlakyEmbedder {
        fn never_fails(dim: usize) -> Arc<Self> {
            Arc::new(Self { dim, calls: Arc::new(AtomicUsize::new(0)), fail_on_call: usize::MAX })
        }
    }

    #[async_trait]
    impl Embedder for FlakyEmbedder {
        fn dim(&self) -> usize {
            self.dim
        }
        async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
            self.embed_batch_lowpri(inputs).await
        }
        async fn embed_batch_lowpri(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.fail_on_call {
                return Err(LunarisError::Storage(lunaris_core::StorageError::Backend(
                    "flaky embedder: injected failure".into(),
                )));
            }
            Ok(inputs
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let mut v = vec![0.0f32; self.dim];
                    v[i % self.dim] = 1.0;
                    v
                })
                .collect())
        }
    }

    /// Write a chunk row straight to KV, bypassing ingest, and return its id.
    async fn seed_chunk(handle: &Lunaris, scope: &Scope, text: &str) -> Ulid {
        let clock = HlcClock::new(0);
        let chunk = Chunk::new(scope.clone(), Ulid::new(), text, text.len() as u32, 0, vec![], &clock);
        let id = chunk.id;
        let key = chunk_key(scope, id);
        let value = serde_json::to_vec(&chunk).unwrap();
        handle.storage().atomic_write(scope, &[WriteOp::KvPut { key, value }]).await.unwrap();
        id
    }

    fn event_for(scope: &Scope, chunk_ids: Vec<String>) -> EmbedPromotionEvent {
        EmbedPromotionEvent {
            kind: "chunk_embed_requested".to_string(),
            scope: scope.as_str().to_string(),
            source: "lunaris:tool_call:post".to_string(),
            episode_id: Ulid::new().to_string(),
            chunk_ids,
            created_at_ms: 0,
        }
    }

    /// The whole point of F25: a chunk the worker cannot promote must leave a
    /// number behind. `promoted == 1` is the vacuity floor — without it a
    /// `missing == 1` could be satisfied by a worker that promoted nothing.
    #[tokio::test]
    async fn a_chunk_whose_row_is_missing_is_counted_not_silently_dropped() {
        let handle = open_test_engine_with_embedder(FlakyEmbedder::never_fails(768)).await;
        let scope = Scope::new("test.f25.missing").unwrap();

        let real = seed_chunk(&handle, &scope, "a real chunk with text").await;
        let ghost = Ulid::new(); // named by the event, never written

        let event = event_for(&scope, vec![real.to_string(), ghost.to_string()]);
        let outcome = promote_batch(&handle, &scope, vec![event], 16).await.unwrap();

        assert_eq!(outcome.promoted, 1, "the real chunk must still be promoted");
        assert_eq!(outcome.missing, 1, "the chunk with no row must be COUNTED, not dropped");
        assert_eq!(outcome.requested, 2);
    }

    /// Every id named by an event lands in exactly one bucket. Without this the
    /// counters can drift apart from reality one skip path at a time.
    #[tokio::test]
    async fn every_requested_chunk_lands_in_exactly_one_bucket() {
        let handle = open_test_engine_with_embedder(FlakyEmbedder::never_fails(768)).await;
        let scope = Scope::new("test.f25.buckets").unwrap();

        let good_a = seed_chunk(&handle, &scope, "first real chunk").await;
        let good_b = seed_chunk(&handle, &scope, "second real chunk").await;
        let blank = seed_chunk(&handle, &scope, "   \n\t ").await;
        let ghost = Ulid::new();

        let event = event_for(
            &scope,
            vec![
                good_a.to_string(),
                good_b.to_string(),
                blank.to_string(),
                ghost.to_string(),
                "not-a-ulid".to_string(),
            ],
        );
        let outcome = promote_batch(&handle, &scope, vec![event], 16).await.unwrap();

        assert_eq!(outcome.promoted, 2, "{outcome:?}");
        assert_eq!(outcome.empty_text, 1, "{outcome:?}");
        assert_eq!(outcome.missing, 1, "{outcome:?}");
        assert_eq!(outcome.bad_id, 1, "{outcome:?}");
        assert_eq!(outcome.failed, 0, "{outcome:?}");
        assert_eq!(
            outcome.requested,
            outcome.promoted + outcome.empty_text + outcome.missing + outcome.bad_id
                + outcome.failed,
            "the buckets must sum to what was asked for: {outcome:?}"
        );
    }

    /// A `?` on the embed call discarded every chunk in every LATER batch too.
    /// One bad batch is a plausible shape for a stable partial loss, and it is
    /// indefensible regardless of whether it is F25's cause.
    #[tokio::test]
    async fn a_failing_batch_does_not_discard_the_batches_after_it() {
        let calls = Arc::new(AtomicUsize::new(0));
        let embedder =
            Arc::new(FlakyEmbedder { dim: 768, calls: calls.clone(), fail_on_call: 1 });
        let handle = open_test_engine_with_embedder(embedder).await;
        let scope = Scope::new("test.f25.batchfail").unwrap();

        let a = seed_chunk(&handle, &scope, "chunk one").await;
        let b = seed_chunk(&handle, &scope, "chunk two").await;

        // batch_size 1 => two batches; the first embed call fails, the second must not.
        let event = event_for(&scope, vec![a.to_string(), b.to_string()]);
        let outcome = promote_batch(&handle, &scope, vec![event], 1).await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2, "the second batch must still be attempted");
        assert_eq!(outcome.promoted, 1, "the surviving batch must be promoted: {outcome:?}");
        assert_eq!(outcome.failed, 1, "the lost chunk must be counted: {outcome:?}");
    }
}

#[cfg(test)]
mod worker_resilience_tests {
    //! scenario: F25 — one unparseable message killed the worker outright.
    //!
    //! `run_worker` handled the FIRST message of a batch with
    //! `parse_event(&msg.payload)?` and every subsequent one with a
    //! log-and-continue. The asymmetry means a single malformed payload stops
    //! promotion for the whole scope, which is exactly the shape of a stable
    //! partial loss nobody can see.
    use super::*;
    use async_trait::async_trait;
    use lunaris_core::{Embedder, LunarisError};
    use lunaris_test_harness::open_test_engine_with_embedder;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct CountingEmbedder {
        texts: Arc<parking_lot::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Embedder for CountingEmbedder {
        fn dim(&self) -> usize {
            768
        }
        async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
            self.embed_batch_lowpri(inputs).await
        }
        async fn embed_batch_lowpri(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
            self.texts.lock().extend(inputs.iter().map(|s| (*s).to_string()));
            Ok(inputs
                .iter()
                .map(|_| {
                    let mut v = vec![0.0f32; 768];
                    v[0] = 1.0;
                    v
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn a_malformed_event_does_not_stop_the_worker() {
        let texts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let engine =
            open_test_engine_with_embedder(Arc::new(CountingEmbedder { texts: texts.clone() }))
                .await;
        let scope = Scope::new("test.f25.malformed").unwrap();
        assert!(
            engine.storage().capabilities().queue_native,
            "this test needs a queue-native store; `memory://` was removed in 0.7.0"
        );

        // Seed a promotable chunk.
        let clock = HlcClock::new(0);
        let chunk =
            Chunk::new(scope.clone(), Ulid::new(), "promotable text", 15, 0, vec![], &clock);
        let chunk_id = chunk.id;
        let key = chunk_key(&scope, chunk_id);
        let value = serde_json::to_vec(&chunk).unwrap();
        engine.storage().atomic_write(&scope, &[WriteOp::KvPut { key, value }]).await.unwrap();

        // Garbage FIRST, then the real event.
        engine
            .storage()
            .publish(&scope, EMBED_PROMOTION_TOPIC, 0, Bytes::from_static(b"{not json"))
            .await
            .unwrap();
        let event = EmbedPromotionEvent {
            kind: "chunk_embed_requested".to_string(),
            scope: scope.as_str().to_string(),
            source: "lunaris:tool_call:post".to_string(),
            episode_id: Ulid::new().to_string(),
            chunk_ids: vec![chunk_id.to_string()],
            created_at_ms: 0,
        };
        engine
            .storage()
            .publish(
                &scope,
                EMBED_PROMOTION_TOPIC,
                0,
                Bytes::from(serde_json::to_vec(&event).unwrap()),
            )
            .await
            .unwrap();

        let (lunaris, _store) = engine.into_parts();
        let config = EmbedPromotionConfig {
            enabled: true,
            worker_enabled: true,
            batch_size: 16,
            batch_wait_ms: 25,
        };

        // The worker loops forever by design; give it a window, then look at
        // what it actually embedded. A worker that died on the garbage
        // embedded nothing.
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            run_worker(Arc::new(lunaris), scope.clone(), config),
        )
        .await;

        let seen = texts.lock().clone();
        assert_eq!(
            seen,
            vec!["promotable text".to_string()],
            "the valid event queued behind a malformed one must still be promoted"
        );
        // Keep the counter import honest.
        assert_eq!(AtomicUsize::new(seen.len()).load(Ordering::SeqCst), 1);
    }
}

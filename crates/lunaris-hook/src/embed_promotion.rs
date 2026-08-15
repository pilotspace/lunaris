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

        if let Err(err) = promote_batch(&handle, &scope, events, config.batch_size).await {
            tracing::debug!(err = %err, "lunaris embed promotion batch failed");
        }
    }
}

async fn promote_batch(
    handle: &Lunaris,
    scope: &Scope,
    events: Vec<EmbedPromotionEvent>,
    batch_size: usize,
) -> anyhow::Result<()> {
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
        return Ok(());
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
        return Ok(());
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
    Ok(())
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

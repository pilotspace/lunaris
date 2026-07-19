//! `memory.feedback` — explicit per-memory ± reinforcement with a reason.
//!
//! INGEST-04 invariant: this handler MUST call `ScopedLunaris::ingest`
//! (or `ScopedLunaris::ingest_idempotent`) and NEVER call `atomic_write`
//! directly for the audit episode. The ledger side effect goes through
//! `ScopedLunaris::record_activation_refs` (the task-2 sanctioned ledger
//! writer) — that is a SEPARATE, non-atomic, best-effort call, not a second
//! `atomic_write` call site.
//! `grep -c 'atomic_write' crates/lunaris-memory-service/src/feedback.rs` must return 0.
//!
//! The `scope` argument is the partition key, bound by the caller (mcp binds
//! it at startup; contextd resolves it per connection). Wire payloads cannot
//! supply or override the scope — CLAUDE.md DTO discipline.
//!
//! Reinforcement effect: `memory.feedback` is the THIRD reinforcement writer
//! in the engram-soul-loop (after `trace_injection`'s weak refs and the
//! citation detector's strong refs) — an explicit human/agent ± vote is
//! declared `Strong` / `StrongNegative` (never `Weak`), moving the memory's
//! ledger weight NOW (visible via `boost_prior` on the next recall) while
//! also leaving a reasoned, durable audit episode for the dream pass
//! (MILESTONE task 8).

use lunaris::{EpisodeBuilder, IngestKind};
use lunaris_core::activation::{Grain, RefSignal, Strength};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::ServiceError;
use lunaris::Lunaris;
use lunaris_core::Scope;

/// Source prefix stamped on every `memory.feedback` audit episode.
pub const FEEDBACK_SOURCE: &str = "lunaris:memory_feedback";

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Explicit ± direction of one feedback call.
///
/// `#[serde(rename_all = "snake_case")]` — the wire values are `"positive"` /
/// `"negative"`; any other string fails deserialization (Reject scenario:
/// unknown sentiment value -> serde reject at the wire, nothing written).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Sentiment {
    Positive,
    Negative,
}

impl Sentiment {
    /// The activation-ledger [`Strength`] this sentiment declares. Positive
    /// feedback is `Strong` (mirrors the citation detector's strong ref);
    /// negative feedback is the new `StrongNegative` variant.
    fn strength(self) -> Strength {
        match self {
            Sentiment::Positive => Strength::Strong,
            Sentiment::Negative => Strength::StrongNegative,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Sentiment::Positive => "positive",
            Sentiment::Negative => "negative",
        }
    }
}

/// Input parameters for `memory.feedback`.
///
/// `#[serde(deny_unknown_fields)]` is mandatory (CLAUDE.md §HTTP DTO discipline).
/// The scope field is absent by design — it is bound at server startup and
/// cannot be overridden by the wire payload.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeedbackParams {
    /// The ULID of the memory (episode/chunk id) being voted on.
    pub memory_id: String,

    /// Explicit ± direction.
    pub sentiment: Sentiment,

    /// Human/agent-supplied reason. Required, non-empty after trim — an
    /// unreasoned vote is rejected so the dream pass (task 8) always has
    /// something to read.
    pub reason: String,

    /// Optional dedupe key (HOOK-05). If present and already seen in this
    /// scope, returns the prior LSN without a second write AND skips the
    /// ledger signal (a replay must not double-count a vote).
    #[serde(default)]
    pub dedupe_key: Option<String>,
}

/// Output of a successful `memory.feedback` call.
///
/// FLAT struct — rmcp aborts server startup if any `#[tool]` response
/// schema root is not `type:"object"` (the 89b9181 story); never wrap this
/// in an enum tag.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeedbackResponse {
    /// Log-sequence number of the committed audit episode (wall_ms:counter).
    pub lsn: String,

    /// True if this call returned a previously-committed LSN (dedupe hit).
    /// When `true` the ledger signal was SKIPPED — replay-safety holds
    /// end-to-end (§3 CONTRACT).
    #[serde(default)]
    pub was_duplicate: bool,

    /// True if the activation-ledger signal was actually applied this call.
    /// `false` when the call was a dedupe replay (signal intentionally
    /// skipped) OR when the ledger write failed (the episode is already
    /// durable — this is an honest partial result, never an `Err`).
    #[serde(default)]
    pub activation_applied: bool,
}

/// The audit episode's content body — everything except `dedupe_key`, which
/// is a transport concern, not memory content.
#[derive(Serialize)]
struct FeedbackPayload<'a> {
    memory_id: &'a str,
    sentiment: Sentiment,
    reason: &'a str,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.feedback`.
///
/// Order (§3 CONTRACT):
/// 1. Validate `memory_id` parses as a ULID and `reason` is non-empty after
///    trim — reject BEFORE any write (Reject scenarios: nothing written).
/// 2. Write the audit episode (source [`FEEDBACK_SOURCE`], content = JSON
///    `{memory_id, sentiment, reason}`, meta `{kind:"feedback", sentiment,
///    memory_id}`) via `ingest` / `ingest_idempotent` (INGEST-04).
/// 3. Unless this was a dedupe replay, apply ONE `RefSignal` (`grain: Turn`,
///    `strength: Strong | StrongNegative`) via `record_activation_refs`. A
///    ledger failure degrades to `activation_applied: false` — the episode
///    already landed, so this handler never returns `Err` past this point.
pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    params: FeedbackParams,
) -> Result<FeedbackResponse, ServiceError> {
    let memory_id = Ulid::from_string(&params.memory_id)
        .map_err(|_| ServiceError::InvalidInput("invalid memory_id".into()))?;
    let reason = params.reason.trim();
    if reason.is_empty() {
        return Err(ServiceError::InvalidInput("reason required".into()));
    }

    let payload =
        FeedbackPayload { memory_id: &params.memory_id, sentiment: params.sentiment, reason };
    let content = serde_json::to_string(&payload)
        .map_err(|e| ServiceError::InvalidInput(format!("serialize feedback payload: {e}")))?;

    let mut meta = serde_json::Map::new();
    meta.insert("kind".into(), serde_json::Value::String("feedback".into()));
    meta.insert(
        "sentiment".into(),
        serde_json::Value::String(params.sentiment.as_str().to_owned()),
    );
    meta.insert("memory_id".into(), serde_json::Value::String(params.memory_id.clone()));

    let mut builder = EpisodeBuilder::new(FEEDBACK_SOURCE, content);
    builder = builder.metadata(meta);

    // Re-derive ScopedLunaris per call — never cache it across calls.
    let scoped = lunaris.scoped(scope.clone());

    let (lsn, was_duplicate) = if let Some(ref key) = params.dedupe_key {
        // HOOK-05 idempotent path: check dedupe key before writing.
        // INGEST-04: the single atomic_write lives inside ingest_idempotent -> ingest.
        let (lsn, kind) = scoped.ingest_idempotent(builder, key).await?;
        (lsn, matches!(kind, IngestKind::Duplicate(_)))
    } else {
        // INGEST-04: the single atomic_write lives inside ScopedLunaris::ingest.
        (scoped.ingest(builder).await?, false)
    };

    // Replay must not double-count (§3 CONTRACT): a dedupe hit skips the
    // ledger signal entirely rather than re-applying it.
    let activation_applied = if was_duplicate {
        false
    } else {
        let signal =
            RefSignal { id: memory_id, grain: Grain::Turn, strength: params.sentiment.strength() };
        match scoped.record_activation_refs(&[signal]).await {
            Ok(()) => true,
            Err(e) => {
                // The episode already landed — a ledger write failure is a
                // best-effort degradation, never an error past this point.
                tracing::warn!(
                    err = %e,
                    scope = scope.as_str(),
                    memory_id = %memory_id,
                    "memory.feedback activation write failed — episode already durable, degrading",
                );
                false
            }
        }
    };

    tracing::debug!(
        scope = scope.as_str(),
        lsn = %lsn,
        was_duplicate,
        activation_applied,
        "memory.feedback committed",
    );

    Ok(FeedbackResponse { lsn: lsn.to_string(), was_duplicate, activation_applied })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt;
    use lunaris_core::activation::ActivationRecord;
    use lunaris_core::{StoragePort, StubEmbedder};

    use super::*;

    async fn fresh(scope_name: &str) -> (Lunaris, Scope) {
        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Lunaris::open_with_embedder("memory://", embedder).await.unwrap();
        let scope = Scope::new(scope_name).unwrap();
        (lunaris, scope)
    }

    async fn read_activation(
        lunaris: &Lunaris,
        scope: &Scope,
        id: Ulid,
    ) -> Option<ActivationRecord> {
        let key = lunaris_core::keyspace::activation_key(scope, id);
        let clock = lunaris_core::HlcClock::new(0);
        lunaris
            .storage()
            .read_as_of(scope, &key, clock.tick())
            .await
            .unwrap()
            .map(|row| serde_json::from_slice(&row.value).unwrap())
    }

    /// Count episodes at rest whose `source == FEEDBACK_SOURCE`.
    async fn feedback_episode_count(lunaris: &Lunaris, scope: &Scope) -> usize {
        let storage = lunaris.storage();
        let mut stream = storage
            .scan_range(scope, &lunaris_core::keyspace::episode_prefix(scope), None)
            .await
            .unwrap();
        let mut n = 0usize;
        while let Some(item) = stream.next().await {
            let (_, value) = item.unwrap();
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&value)
                && v.get("source").and_then(|s| s.as_str()) == Some(FEEDBACK_SOURCE)
            {
                n += 1;
            }
        }
        n
    }

    /// Total episode count in the scope, regardless of source — used by the
    /// Reject scenarios to prove "nothing written".
    async fn scope_episode_total(lunaris: &Lunaris, scope: &Scope) -> usize {
        let storage = lunaris.storage();
        let mut stream = storage
            .scan_range(scope, &lunaris_core::keyspace::episode_prefix(scope), None)
            .await
            .unwrap();
        let mut n = 0usize;
        while let Some(item) = stream.next().await {
            item.unwrap();
            n += 1;
        }
        n
    }

    /// Scenario 1 — positive feedback strengthens the ledger and writes the
    /// audit episode.
    #[tokio::test]
    async fn positive_feedback_strengthens_ledger_and_writes_audit_episode() {
        let (lunaris, scope) = fresh("test-feedback-positive").await;
        let memory_id = Ulid::new();
        // Seed one weak ref so the record starts at weighted == 1.0.
        lunaris
            .scoped(scope.clone())
            .record_activation_refs(&[RefSignal {
                id: memory_id,
                grain: Grain::Turn,
                strength: Strength::Weak,
            }])
            .await
            .unwrap();

        let params = FeedbackParams {
            memory_id: memory_id.to_string(),
            sentiment: Sentiment::Positive,
            reason: "used verbatim".into(),
            dedupe_key: None,
        };
        let resp = handle(&lunaris, &scope, params).await.expect("positive feedback must succeed");
        assert!(!resp.lsn.is_empty());
        assert!(!resp.was_duplicate);
        assert!(resp.activation_applied);

        let record =
            read_activation(&lunaris, &scope, memory_id).await.expect("ledger record must exist");
        assert_eq!(record.weighted, 4.0);
        assert_eq!(record.last_strength, Strength::Strong);

        assert_eq!(feedback_episode_count(&lunaris, &scope).await, 1);
    }

    /// Scenario 2 — negative feedback weakens the ledger, floored at zero.
    #[tokio::test]
    async fn negative_feedback_floors_ledger_at_zero() {
        let (lunaris, scope) = fresh("test-feedback-negative").await;
        let memory_id = Ulid::new();
        lunaris
            .scoped(scope.clone())
            .record_activation_refs(&[RefSignal {
                id: memory_id,
                grain: Grain::Turn,
                strength: Strength::Weak,
            }])
            .await
            .unwrap();

        let params = FeedbackParams {
            memory_id: memory_id.to_string(),
            sentiment: Sentiment::Negative,
            reason: "misleading".into(),
            dedupe_key: None,
        };
        let resp = handle(&lunaris, &scope, params).await.expect("negative feedback must succeed");
        assert!(resp.activation_applied);

        let record =
            read_activation(&lunaris, &scope, memory_id).await.expect("ledger record must exist");
        assert_eq!(record.weighted, 0.0);
        assert_eq!(record.last_strength, Strength::StrongNegative);
        assert_eq!(feedback_episode_count(&lunaris, &scope).await, 1);
    }

    /// Scenario 3 — invalid memory_id writes nothing.
    #[tokio::test]
    async fn invalid_memory_id_rejected_writes_nothing() {
        let (lunaris, scope) = fresh("test-feedback-bad-id").await;
        let params = FeedbackParams {
            memory_id: "not-a-ulid".into(),
            sentiment: Sentiment::Positive,
            reason: "whatever".into(),
            dedupe_key: None,
        };
        let err = handle(&lunaris, &scope, params).await.expect_err("bad ULID must be rejected");
        assert!(matches!(err, ServiceError::InvalidInput(_)));
        assert_eq!(scope_episode_total(&lunaris, &scope).await, 0);
    }

    /// Scenario 4 — empty reason writes nothing.
    #[tokio::test]
    async fn empty_reason_rejected_writes_nothing() {
        let (lunaris, scope) = fresh("test-feedback-empty-reason").await;
        let params = FeedbackParams {
            memory_id: Ulid::new().to_string(),
            sentiment: Sentiment::Positive,
            reason: "   ".into(),
            dedupe_key: None,
        };
        let err =
            handle(&lunaris, &scope, params).await.expect_err("empty reason must be rejected");
        assert!(matches!(err, ServiceError::InvalidInput(_)));
        assert_eq!(scope_episode_total(&lunaris, &scope).await, 0);
    }

    /// Scenario 8 — dedupe replay returns the prior LSN and the ledger
    /// gains the signal only once.
    #[tokio::test]
    async fn dedupe_replay_applies_ledger_signal_only_once() {
        let (lunaris, scope) = fresh("test-feedback-dedupe").await;
        let memory_id = Ulid::new();
        let make_params = || FeedbackParams {
            memory_id: memory_id.to_string(),
            sentiment: Sentiment::Positive,
            reason: "used verbatim".into(),
            dedupe_key: Some("feedback-dedupe-1".into()),
        };

        let first = handle(&lunaris, &scope, make_params()).await.unwrap();
        assert!(!first.was_duplicate);
        assert!(first.activation_applied);

        let second = handle(&lunaris, &scope, make_params()).await.unwrap();
        assert!(second.was_duplicate, "replay must return was_duplicate = true");
        assert_eq!(second.lsn, first.lsn, "replay must return the prior LSN");
        assert!(!second.activation_applied, "replay must skip the ledger signal");

        let record =
            read_activation(&lunaris, &scope, memory_id).await.expect("ledger record must exist");
        assert_eq!(record.n, 1, "the ledger must gain the signal only once across the replay");
        assert_eq!(record.weighted, 3.0);
    }

    /// Scenario 5 — activation write failure degrades honestly: the episode
    /// is written, the response is Ok with `activation_applied == false`.
    #[tokio::test]
    async fn activation_write_failure_degrades_honestly_episode_still_written() {
        let scope = Scope::new("test-feedback-activation-fail").unwrap();
        let inner = lunaris::open("memory://").await.unwrap();
        let failing = Arc::new(ActivationFailingStorage { inner }) as Arc<dyn StoragePort>;
        let embedder: Arc<dyn lunaris_core::Embedder> = Arc::new(StubEmbedder::new(768));
        let clock = lunaris_core::HlcClock::new(0);
        let lunaris = Lunaris::with_parts(failing, embedder, clock);

        let params = FeedbackParams {
            memory_id: Ulid::new().to_string(),
            sentiment: Sentiment::Positive,
            reason: "used verbatim".into(),
            dedupe_key: None,
        };
        let resp =
            handle(&lunaris, &scope, params).await.expect("episode write must still succeed");
        assert!(!resp.lsn.is_empty());
        assert!(
            !resp.activation_applied,
            "activation write failure must degrade honestly, not error"
        );
        assert_eq!(feedback_episode_count(&lunaris, &scope).await, 1);
    }

    /// `StoragePort` that fails only `atomic_write` batches touching an
    /// activation-ledger key, delegating everything else to `inner`. Mirrors
    /// `lunaris-hook::context.rs`'s identical test double (the citation
    /// detector's activation-write-is-best-effort proof) — copied rather
    /// than shared because it is `#[cfg(test)]`-local to each crate.
    struct ActivationFailingStorage {
        inner: Arc<dyn StoragePort>,
    }

    #[async_trait::async_trait]
    impl StoragePort for ActivationFailingStorage {
        async fn atomic_write(
            &self,
            scope: &Scope,
            ops: &[lunaris_core::WriteOp],
        ) -> Result<lunaris_core::Lsn, lunaris_core::StorageError> {
            let touches_activation = ops.iter().any(|op| {
                let key = match op {
                    lunaris_core::WriteOp::KvPut { key, .. } => key,
                    lunaris_core::WriteOp::KvDelete { key } => key,
                    _ => return false,
                };
                key.windows(b":activation:".len()).any(|w| w == b":activation:")
            });
            if touches_activation {
                return Err(lunaris_core::StorageError::Backend(
                    "forced activation write failure (test)".into(),
                ));
            }
            self.inner.atomic_write(scope, ops).await
        }

        #[allow(clippy::too_many_arguments)]
        async fn vector_search(
            &self,
            scope: &Scope,
            index: &str,
            query: &[f32],
            k: usize,
            filter: Option<&lunaris_core::Filter>,
            as_of: Option<lunaris_core::Hlc>,
            rerank: bool,
        ) -> Result<Vec<lunaris_core::VectorHit>, lunaris_core::StorageError> {
            self.inner.vector_search(scope, index, query, k, filter, as_of, rerank).await
        }

        async fn graph_traverse(
            &self,
            scope: &Scope,
            query: &lunaris_core::CypherQuery,
            as_of: Option<lunaris_core::Hlc>,
        ) -> Result<lunaris_core::GraphResult, lunaris_core::StorageError> {
            self.inner.graph_traverse(scope, query, as_of).await
        }

        async fn scan_range(
            &self,
            scope: &Scope,
            prefix: &[u8],
            as_of: Option<lunaris_core::Hlc>,
        ) -> Result<
            futures::stream::BoxStream<
                '_,
                Result<(bytes::Bytes, bytes::Bytes), lunaris_core::StorageError>,
            >,
            lunaris_core::StorageError,
        > {
            self.inner.scan_range(scope, prefix, as_of).await
        }

        async fn read_as_of(
            &self,
            scope: &Scope,
            key: &[u8],
            as_of: lunaris_core::Hlc,
        ) -> Result<Option<lunaris_core::Row<bytes::Bytes>>, lunaris_core::StorageError> {
            self.inner.read_as_of(scope, key, as_of).await
        }

        async fn publish(
            &self,
            scope: &Scope,
            topic: &str,
            partition: u16,
            payload: bytes::Bytes,
        ) -> Result<u64, lunaris_core::StorageError> {
            self.inner.publish(scope, topic, partition, payload).await
        }

        async fn subscribe(
            &self,
            scope: &Scope,
            group: &str,
            topic: &str,
            partition: u16,
        ) -> Result<
            futures::stream::BoxStream<
                'static,
                Result<lunaris_core::QueueMsg, lunaris_core::StorageError>,
            >,
            lunaris_core::StorageError,
        > {
            self.inner.subscribe(scope, group, topic, partition).await
        }

        fn capabilities(&self) -> lunaris_core::StorageCapabilities {
            self.inner.capabilities()
        }
    }

    /// Collect the trailing `{ulid}` of every KV key under `prefix`.
    async fn scan_trailing_ulids(lunaris: &Lunaris, scope: &Scope, prefix: Vec<u8>) -> Vec<Ulid> {
        let mut out = Vec::new();
        let storage = lunaris.storage();
        let mut stream = storage.scan_range(scope, &prefix, None).await.unwrap();
        while let Some(item) = stream.next().await {
            let (key, _) = item.unwrap();
            let s = std::str::from_utf8(&key).unwrap();
            let tail = &s[s.rfind(':').unwrap() + 1..];
            if let Ok(id) = Ulid::from_string(tail) {
                out.push(id);
            }
        }
        out
    }

    /// engram id-space fix (2026-07-19 live finding) — a vote on a CHUNK id
    /// must land on the PARENT EPISODE's ledger row. Recall historically
    /// returned chunk ULIDs labeled `episode_id`; a feedback vote on such an
    /// id wrote a chunk-keyed activation row that `memory.dream_agenda`
    /// (episode-only hydration) silently dropped. The handler must resolve
    /// chunk -> parent episode before emitting the `RefSignal` so the ledger
    /// stays episode-grained regardless of caller vintage.
    #[tokio::test]
    async fn feedback_on_chunk_id_lands_on_parent_episode_ledger_row() {
        let (lunaris, scope) = fresh("test-feedback-chunk-resolution").await;
        let scoped = lunaris.scoped(scope.clone());
        scoped
            .ingest(lunaris::EpisodeBuilder::new("test/src", "chunk-grain feedback fixture"))
            .await
            .unwrap();

        let episode_ids =
            scan_trailing_ulids(&lunaris, &scope, lunaris_core::keyspace::episode_prefix(&scope))
                .await;
        let chunk_ids =
            scan_trailing_ulids(&lunaris, &scope, lunaris_core::keyspace::chunk_prefix(&scope))
                .await;
        assert_eq!(episode_ids.len(), 1, "exactly one episode ingested");
        let episode_id = episode_ids[0];
        let chunk_id = *chunk_ids.first().expect("ingest must have produced a chunk");
        assert_ne!(episode_id, chunk_id);

        let resp = handle(
            &lunaris,
            &scope,
            FeedbackParams {
                memory_id: chunk_id.to_string(),
                sentiment: Sentiment::Positive,
                reason: "voted via a recall-hit chunk id".into(),
                dedupe_key: None,
            },
        )
        .await
        .unwrap();
        assert!(resp.activation_applied);

        let episode_row = read_activation(&lunaris, &scope, episode_id).await;
        assert!(
            episode_row.is_some(),
            "the ledger signal must resolve chunk -> parent episode and land episode-keyed"
        );
        assert_eq!(episode_row.unwrap().last_strength, lunaris_core::activation::Strength::Strong);
        assert!(
            read_activation(&lunaris, &scope, chunk_id).await.is_none(),
            "no chunk-keyed ledger row may be written — dream-agenda cannot hydrate it"
        );
    }
}

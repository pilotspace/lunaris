//! Codex memory context sidecar protocol and rendering helpers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use lunaris::{Lunaris, Query};
use lunaris_core::snippet::{parse_jsonish, single_line, summarize_json, trim_to_chars};
use lunaris_core::{Episode, HlcClock, Lsn, NoopEmbedder, Scope, StoragePort, StubEmbedder};
use lunaris_retrieve::{
    AndRetriever, FuseRrfRetriever, Keyword, QueryContext, RawHit, Retriever, SourceOp, Vector,
    hydrate, hydrate_mixed,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::embed_promotion::{self, EmbedPromotionConfig};
use crate::scrub::ScrubEngine;

pub const DEFAULT_PROMPT_MAX_HITS: usize = 5;
pub const DEFAULT_PROMPT_MAX_CHARS: usize = 1600;
pub const DEFAULT_PROMPT_MIN_SCORE: f32 = 0.55;
pub const DEFAULT_TOOL_MAX_HITS: usize = 3;
pub const DEFAULT_TOOL_MAX_CHARS: usize = 900;
pub const DEFAULT_TOOL_MIN_SCORE: f32 = 0.60;
/// hook-recall-graph-hybrid contract v1: hard budget around the fused root's
/// retrieve (`LUNARIS_CONTEXT_RECALL_TIMEOUT_MS` override). Embedding runs
/// OUTSIDE this window — a cold GGUF load must not eat the recall budget.
pub const DEFAULT_HYBRID_TIMEOUT_MS: u64 = 1500;

/// The hook's hybrid recall root (hook-recall-graph-hybrid contract v1.1):
///
/// `Vector("chunks",k) ∧ Keyword::bm25("chunks",k) ∧ Vector("facts",k)
///  ∧ Keyword::bm25("facts",k) → fuse_rrf(60)`
///
/// The facts BM25 leg is the reliable fact signal today — graph-ON ingest
/// writes STUB fact embeddings (det_vec), so in the merged vector RRF group
/// facts would starve at scale; `fact_text` is FT-indexed as `content` and
/// ranks lexically. The vector facts leg stays wired for the real-fact-
/// embeddings follow-on. Runs client-side RRF deterministically: a manually
/// built `QueryContext::new` has `moon_storage = None`, so the Moon-native
/// FT.HYBRID dispatch never fires here.
pub fn hybrid_root(k: usize) -> FuseRrfRetriever {
    let chunks = Vector::new("chunks", k).and(Keyword::bm25("chunks", k));
    let facts = Vector::new("facts", k).and(Keyword::bm25("facts", k));
    AndRetriever::new(Box::new(chunks), Box::new(facts)).fuse_rrf(60)
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextRequest {
    RecallForPrompt {
        cwd: Option<PathBuf>,
        scope: Option<String>,
        session_id: Option<String>,
        prompt: String,
        max_hits: Option<usize>,
        max_chars: Option<usize>,
        min_score: Option<f32>,
    },
    RecallAfterTool {
        cwd: Option<PathBuf>,
        scope: Option<String>,
        session_id: Option<String>,
        tool: Option<String>,
        summary: String,
        paths: Option<Vec<String>>,
        max_hits: Option<usize>,
        max_chars: Option<usize>,
        min_score: Option<f32>,
    },
    CaptureToolCall {
        cwd: Option<PathBuf>,
        scope: Option<String>,
        session_id: Option<String>,
        tool: Option<String>,
        payload: Value,
    },
    CaptureToolResult {
        cwd: Option<PathBuf>,
        scope: Option<String>,
        session_id: Option<String>,
        tool: Option<String>,
        payload: Value,
    },
    TurnFeedback {
        cwd: Option<PathBuf>,
        scope: Option<String>,
        session_id: Option<String>,
        injected_memory_ids: Vec<String>,
        outcome: Option<String>,
    },
    Health,
}

#[derive(Clone, Debug, Serialize)]
pub struct ContextMemory {
    pub episode_id: String,
    pub source: String,
    pub score: f32,
    pub snippet: String,
}

#[derive(Debug, Serialize)]
pub struct ContextResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub injection_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<ContextMemory>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rendered_context: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsn: Option<Lsn>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ContextResponse {
    pub fn empty() -> Self {
        Self {
            ok: true,
            injection_id: None,
            memories: vec![],
            rendered_context: String::new(),
            lsn: None,
            error: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            injection_id: None,
            memories: vec![],
            rendered_context: String::new(),
            lsn: None,
            error: Some(message.into()),
        }
    }
}

#[derive(Clone)]
pub struct ContextService {
    handles: Arc<Mutex<HashMap<String, Arc<Lunaris>>>>,
    storages: Arc<Mutex<HashMap<String, Arc<dyn StoragePort>>>>,
    embed_workers: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
    query_embeddings: Arc<Mutex<HashMap<String, Vec<f32>>>>,
}

impl ContextService {
    pub fn new() -> Self {
        Self {
            handles: Arc::new(Mutex::new(HashMap::new())),
            storages: Arc::new(Mutex::new(HashMap::new())),
            embed_workers: Arc::new(Mutex::new(HashMap::new())),
            query_embeddings: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn handle(&self, request: ContextRequest) -> ContextResponse {
        match self.handle_inner(request).await {
            Ok(response) => response,
            Err(err) => ContextResponse::error(err.to_string()),
        }
    }

    async fn handle_inner(&self, request: ContextRequest) -> anyhow::Result<ContextResponse> {
        match request {
            ContextRequest::Health => Ok(ContextResponse::empty()),
            ContextRequest::RecallForPrompt {
                cwd,
                scope,
                session_id,
                prompt,
                max_hits,
                max_chars,
                min_score,
            } => {
                let scope = resolve_scope(cwd.as_deref(), scope.as_deref())?;
                let max_hits = max_hits
                    .or_else(|| {
                        env_usize_any(&[
                            "LUNARIS_CONTEXT_MAX_HITS",
                            "LUNARIS_CODEX_CONTEXT_MAX_HITS",
                        ])
                    })
                    .unwrap_or(DEFAULT_PROMPT_MAX_HITS);
                let max_chars = max_chars
                    .or_else(|| {
                        env_usize_any(&[
                            "LUNARIS_CONTEXT_MAX_CHARS",
                            "LUNARIS_CODEX_CONTEXT_MAX_CHARS",
                        ])
                    })
                    .unwrap_or(DEFAULT_PROMPT_MAX_CHARS);
                let min_score = min_score
                    .or_else(|| {
                        env_f32_any(&[
                            "LUNARIS_CONTEXT_MIN_SCORE",
                            "LUNARIS_CODEX_CONTEXT_MIN_SCORE",
                        ])
                    })
                    .unwrap_or(DEFAULT_PROMPT_MIN_SCORE);
                self.recall_and_trace(
                    &scope,
                    &prompt,
                    "prompt",
                    session_id.as_deref(),
                    max_hits,
                    max_chars,
                    min_score,
                    None,
                )
                .await
            }
            ContextRequest::RecallAfterTool {
                cwd,
                scope,
                session_id,
                tool,
                summary,
                paths,
                max_hits,
                max_chars,
                min_score,
            } => {
                let scope = resolve_scope(cwd.as_deref(), scope.as_deref())?;
                let max_hits = max_hits
                    .or_else(|| {
                        env_usize_any(&[
                            "LUNARIS_CONTEXT_POST_TOOL_MAX_HITS",
                            "LUNARIS_CONTEXT_MAX_HITS",
                            "LUNARIS_CODEX_POST_TOOL_MAX_HITS",
                        ])
                    })
                    .unwrap_or(DEFAULT_TOOL_MAX_HITS);
                let max_chars = max_chars
                    .or_else(|| {
                        env_usize_any(&[
                            "LUNARIS_CONTEXT_POST_TOOL_MAX_CHARS",
                            "LUNARIS_CONTEXT_MAX_CHARS",
                            "LUNARIS_CODEX_POST_TOOL_MAX_CHARS",
                        ])
                    })
                    .unwrap_or(DEFAULT_TOOL_MAX_CHARS);
                let min_score = min_score
                    .or_else(|| {
                        env_f32_any(&[
                            "LUNARIS_CONTEXT_POST_TOOL_MIN_SCORE",
                            "LUNARIS_CONTEXT_MIN_SCORE",
                            "LUNARIS_CODEX_POST_TOOL_MIN_SCORE",
                        ])
                    })
                    .unwrap_or(DEFAULT_TOOL_MIN_SCORE);
                let tool_context = ToolContext { tool, paths };
                self.recall_and_trace(
                    &scope,
                    &summary,
                    "post_tool",
                    session_id.as_deref(),
                    max_hits,
                    max_chars,
                    min_score,
                    Some(tool_context),
                )
                .await
            }
            ContextRequest::CaptureToolCall { cwd, scope, session_id, tool, payload } => {
                let scope = resolve_scope(cwd.as_deref(), scope.as_deref())?;
                self.spawn_capture_tool(&scope, "codex:tool_call:pre", session_id, tool, payload);
                Ok(ContextResponse::empty())
            }
            ContextRequest::CaptureToolResult { cwd, scope, session_id, tool, payload } => {
                let scope = resolve_scope(cwd.as_deref(), scope.as_deref())?;
                self.spawn_capture_tool(&scope, "codex:tool_call:post", session_id, tool, payload);
                Ok(ContextResponse::empty())
            }
            ContextRequest::TurnFeedback {
                cwd,
                scope,
                session_id,
                injected_memory_ids,
                outcome,
            } => {
                let scope = resolve_scope(cwd.as_deref(), scope.as_deref())?;
                self.capture_feedback(&scope, session_id, injected_memory_ids, outcome).await
            }
        }
    }

    async fn handle_for_scope(&self, scope: &Scope) -> anyhow::Result<Arc<Lunaris>> {
        let key = scope.as_str().to_owned();
        if let Some(existing) = self.handles.lock().await.get(&key).cloned() {
            return Ok(existing);
        }

        let storage_url = crate::scope::resolve_storage_url(scope)?;
        let handle = Arc::new(Lunaris::open(&storage_url).await?);
        let mut handles = self.handles.lock().await;
        Ok(handles.entry(key).or_insert(handle).clone())
    }

    async fn storage_for_scope(&self, scope: &Scope) -> anyhow::Result<Arc<dyn StoragePort>> {
        let key = scope.as_str().to_owned();
        if let Some(existing) = self.storages.lock().await.get(&key).cloned() {
            return Ok(existing);
        }

        let storage_url = crate::scope::resolve_storage_url(scope)?;
        let storage = lunaris::open(&storage_url).await?;
        let mut storages = self.storages.lock().await;
        Ok(storages.entry(key).or_insert(storage).clone())
    }

    #[allow(clippy::too_many_arguments)]
    async fn recall_and_trace(
        &self,
        scope: &Scope,
        text: &str,
        phase: &str,
        session_id: Option<&str>,
        max_hits: usize,
        max_chars: usize,
        min_score: f32,
        tool_context: Option<ToolContext>,
    ) -> anyhow::Result<ContextResponse> {
        if text.trim().is_empty() {
            return Ok(ContextResponse::empty());
        }

        let handle = self.handle_for_scope(scope).await?;
        let mut query = Query::text(text);
        let candidate_k = max_hits.saturating_mul(4).max(max_hits).max(1);
        query.k = candidate_k;

        // hook-recall-graph-hybrid contract v1: hybrid is the DEFAULT;
        // `LUNARIS_CONTEXT_RECALL=vector` restores the legacy path exactly.
        // ANY hybrid timeout/error/empty degrades to the legacy path below —
        // a recall failure must never surface to the agent.
        let recall_mode =
            std::env::var("LUNARIS_CONTEXT_RECALL").unwrap_or_else(|_| "hybrid".to_owned());
        let started = Instant::now();
        let mut hybrid_memories: Option<Vec<ContextMemory>> = None;
        if recall_mode != "vector" {
            match self.recall_hybrid_hot_path(&handle, scope, &query, candidate_k).await {
                Ok(hits) if !hits.is_empty() => {
                    // Fused candidates BYPASS the raw min_score cosine
                    // threshold — RRF rank (Σ 1/(60+rank) ≈ 0.03-scale) is
                    // the quality signal; 0.55 would annihilate every hit.
                    // The keyword sidecar merge is SKIPPED: BM25 is already
                    // a fused leg.
                    let candidates: Vec<ContextMemory> = hits
                        .into_iter()
                        .map(|h| ContextMemory {
                            episode_id: ulid_bytes_to_string(&h.id),
                            source: h.source,
                            score: h.score,
                            snippet: scrub_and_trim(&h.text, 900),
                        })
                        .collect();
                    hybrid_memories = Some(curate_context_memories_lossy(candidates, max_hits));
                }
                Ok(_) => {
                    tracing::warn!(phase, "lunaris hybrid recall empty; legacy fallback used");
                }
                Err(err) => {
                    tracing::warn!(
                        phase,
                        err = %err,
                        "lunaris hybrid recall failed; legacy fallback used"
                    );
                }
            }
        }
        if let Some(memories) = hybrid_memories {
            let recall_elapsed_ms = started.elapsed().as_millis();
            if std::env::var("LUNARIS_CONTEXT_PROFILE").ok().as_deref() == Some("1") {
                tracing::info!(
                    phase,
                    candidate_k,
                    recall_elapsed_ms,
                    "lunaris context recall hybrid hot path"
                );
            }
            return self
                .finish_recall(scope, phase, session_id, max_chars, tool_context, memories)
                .await;
        }

        let hits =
            self.recall_hot_path_with_keyword_fallback(&handle, scope, &query, candidate_k).await?;
        let recall_elapsed_ms = started.elapsed().as_millis();
        if std::env::var("LUNARIS_CONTEXT_PROFILE").ok().as_deref() == Some("1") {
            tracing::info!(
                phase,
                candidate_k,
                recall_elapsed_ms,
                "lunaris context recall hot path"
            );
        }

        let mut candidates: Vec<ContextMemory> = hits
            .into_iter()
            .filter(|h| h.score >= min_score)
            .map(|h| ContextMemory {
                episode_id: ulid_bytes_to_string(&h.id),
                source: h.source,
                score: h.score,
                snippet: scrub_and_trim(&h.text, 900),
            })
            .collect();
        let keyword_candidates: Vec<ContextMemory> = match self
            .recall_keyword_hot_path(&handle, scope, &query, candidate_k)
            .await
        {
            Ok(keyword_hits) => {
                if std::env::var("LUNARIS_CONTEXT_PROFILE").ok().as_deref() == Some("1") {
                    tracing::info!(hits = keyword_hits.len(), "lunaris context keyword sidecar");
                }
                keyword_hits
                    .into_iter()
                    .filter(|h| h.score >= min_score)
                    .map(|h| ContextMemory {
                        episode_id: ulid_bytes_to_string(&h.id),
                        source: h.source,
                        score: h.score,
                        snippet: scrub_and_trim(&h.text, 900),
                    })
                    .collect()
            }
            Err(err) => {
                tracing::debug!(err = %err, "lunaris context keyword sidecar failed");
                Vec::new()
            }
        };
        if candidates.is_empty() && !keyword_candidates.is_empty() {
            if std::env::var("LUNARIS_CONTEXT_PROFILE").ok().as_deref() == Some("1") {
                tracing::info!(
                    hits = keyword_candidates.len(),
                    "lunaris context keyword fallback after score filter"
                );
            }
            candidates = keyword_candidates.clone();
        }
        let mut memories = curate_context_memories_lossy(keyword_candidates, max_hits);
        if memories.len() < max_hits {
            let mut seen_ids: HashSet<String> =
                memories.iter().map(|m| m.episode_id.clone()).collect();
            for memory in curate_context_memories(candidates, max_hits) {
                if seen_ids.insert(memory.episode_id.clone()) {
                    memories.push(memory);
                }
                if memories.len() >= max_hits {
                    break;
                }
            }
        }
        if memories.is_empty() {
            let keyword_hits =
                self.recall_keyword_hot_path(&handle, scope, &query, candidate_k).await?;
            if std::env::var("LUNARIS_CONTEXT_PROFILE").ok().as_deref() == Some("1") {
                tracing::info!(
                    hits = keyword_hits.len(),
                    "lunaris context keyword fallback after curation"
                );
            }
            let keyword_candidates: Vec<ContextMemory> = keyword_hits
                .into_iter()
                .filter(|h| h.score >= min_score)
                .map(|h| ContextMemory {
                    episode_id: ulid_bytes_to_string(&h.id),
                    source: h.source,
                    score: h.score,
                    snippet: scrub_and_trim(&h.text, 900),
                })
                .collect();
            memories = curate_context_memories_lossy(keyword_candidates, max_hits);
        }

        self.finish_recall(scope, phase, session_id, max_chars, tool_context, memories).await
    }

    /// Shared response tail for BOTH recall paths: empty short-circuit,
    /// render, fire-and-forget injection trace.
    async fn finish_recall(
        &self,
        scope: &Scope,
        phase: &str,
        session_id: Option<&str>,
        max_chars: usize,
        tool_context: Option<ToolContext>,
        memories: Vec<ContextMemory>,
    ) -> anyhow::Result<ContextResponse> {
        if memories.is_empty() {
            return Ok(ContextResponse::empty());
        }

        let injection_id = ulid::Ulid::new().to_string();
        let rendered_context = render_context(phase, tool_context.as_ref(), &memories, max_chars);
        self.spawn_trace_injection(
            scope,
            injection_id.clone(),
            phase,
            session_id.map(str::to_owned),
            memories.iter().map(|m| m.episode_id.clone()).collect(),
        );

        Ok(ContextResponse {
            ok: true,
            injection_id: Some(injection_id),
            memories,
            rendered_context,
            lsn: None,
            error: None,
        })
    }

    /// hook-recall-graph-hybrid contract v1.1 — the fused hybrid hot path.
    ///
    /// The query embedding comes from the hook's embed cache and is computed
    /// OUTSIDE the timeout window (a cold GGUF load must not eat the recall
    /// budget); the `QueryContext` gets it PRE-SEEDED, so the `StubEmbedder`
    /// placeholder is provably never invoked (`embed_once` is
    /// `get_or_try_init` on a set cell). Hydration is fact-aware
    /// (`hydrate_mixed`): fact hits render as `text = fact_text`,
    /// `source = "fact:{predicate}"`.
    async fn recall_hybrid_hot_path(
        &self,
        handle: &Lunaris,
        scope: &Scope,
        query: &Query,
        candidate_k: usize,
    ) -> anyhow::Result<Vec<lunaris_retrieve::Hit>> {
        let embedding = self.cached_query_embedding(handle, scope, &query.text).await?;
        let ctx = QueryContext::new(
            query.clone(),
            scope.clone(),
            Arc::new(StubEmbedder::new(embedding.len())),
            handle.storage(),
            handle.keyword(),
        );
        ctx.query_embedding
            .set(embedding)
            .map_err(|_| anyhow::anyhow!("query_embedding OnceCell already seeded"))?;

        let timeout_ms = std::env::var("LUNARIS_CONTEXT_RECALL_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_HYBRID_TIMEOUT_MS);
        let root = hybrid_root(candidate_k);
        let raw_hits =
            tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), root.retrieve(&ctx))
                .await
                .map_err(|_| anyhow::anyhow!("hybrid recall timed out after {timeout_ms}ms"))??;
        Ok(hydrate_mixed(handle.storage().as_ref(), scope, raw_hits, query.as_of, false).await?)
    }

    async fn recall_vector_hot_path(
        &self,
        handle: &Lunaris,
        scope: &Scope,
        query: &Query,
        candidate_k: usize,
    ) -> anyhow::Result<Vec<lunaris_retrieve::Hit>> {
        let embedding = self.cached_query_embedding(handle, scope, &query.text).await?;
        let raw_hits = handle
            .storage()
            .vector_search(
                scope,
                "chunks",
                &embedding,
                candidate_k,
                query.filter.as_ref(),
                query.as_of,
                false,
            )
            .await?
            .into_iter()
            .map(|hit| RawHit {
                id: hit.id,
                score: hit.score,
                rerank_applied: hit.rerank_applied,
                degraded: false,
                metadata: hit.metadata,
                source_op: SourceOp::Vector,
            })
            .collect();
        Ok(hydrate(handle.storage().as_ref(), scope, raw_hits, query.as_of, false).await?)
    }

    async fn recall_hot_path_with_keyword_fallback(
        &self,
        handle: &Lunaris,
        scope: &Scope,
        query: &Query,
        candidate_k: usize,
    ) -> anyhow::Result<Vec<lunaris_retrieve::Hit>> {
        match self.recall_vector_hot_path(handle, scope, query, candidate_k).await {
            Ok(hits) if !hits.is_empty() => Ok(hits),
            Ok(_) => {
                let hits = self.recall_keyword_hot_path(handle, scope, query, candidate_k).await?;
                if std::env::var("LUNARIS_CONTEXT_PROFILE").ok().as_deref() == Some("1") {
                    tracing::info!(hits = hits.len(), "lunaris context keyword fallback");
                }
                Ok(hits)
            }
            Err(vector_err) => {
                match self.recall_keyword_hot_path(handle, scope, query, candidate_k).await {
                    Ok(hits) => {
                        tracing::debug!(
                            err = %vector_err,
                            hits = hits.len(),
                            "lunaris context vector recall failed; keyword fallback used"
                        );
                        Ok(hits)
                    }
                    Err(keyword_err) => {
                        Err(vector_err
                            .context(format!("keyword fallback also failed: {keyword_err}")))
                    }
                }
            }
        }
    }

    async fn recall_keyword_hot_path(
        &self,
        handle: &Lunaris,
        scope: &Scope,
        query: &Query,
        candidate_k: usize,
    ) -> anyhow::Result<Vec<lunaris_retrieve::Hit>> {
        let raw_hits = handle
            .keyword()
            .keyword_search(
                scope,
                "chunks",
                &query.text,
                candidate_k,
                query.filter.as_ref(),
                query.as_of,
            )
            .await?
            .into_iter()
            .map(|hit| RawHit {
                id: hit.id,
                score: hit.score,
                rerank_applied: false,
                degraded: false,
                metadata: hit.metadata,
                source_op: SourceOp::Keyword,
            })
            .collect();
        Ok(hydrate(handle.storage().as_ref(), scope, raw_hits, query.as_of, false).await?)
    }

    async fn cached_query_embedding(
        &self,
        handle: &Lunaris,
        scope: &Scope,
        text: &str,
    ) -> anyhow::Result<Vec<f32>> {
        let key = format!("{}:{}", scope.as_str(), stable_query_key(text));
        if let Some(cached) = self.query_embeddings.lock().await.get(&key).cloned() {
            return Ok(cached);
        }

        let started = Instant::now();
        let mut rows = handle.embedder().embed_batch(&[text]).await?;
        let embedding = rows
            .pop()
            .ok_or_else(|| anyhow::anyhow!("embedder returned no row for context query"))?;
        let elapsed_ms = started.elapsed().as_millis();
        if std::env::var("LUNARIS_CONTEXT_PROFILE").ok().as_deref() == Some("1") {
            tracing::info!(elapsed_ms, "lunaris context query embedded");
        }

        let mut cache = self.query_embeddings.lock().await;
        if cache.len() >= env_usize_any(&["LUNARIS_CONTEXT_EMBED_CACHE_MAX"]).unwrap_or(256) {
            cache.clear();
        }
        cache.insert(key, embedding.clone());
        Ok(embedding)
    }

    async fn capture_tool(
        &self,
        scope: &Scope,
        source: &str,
        session_id: Option<String>,
        tool: Option<String>,
        payload: Value,
    ) -> anyhow::Result<ContextResponse> {
        let content = summarize_json_payload(&payload, 4000);
        let mut meta = Map::new();
        if let Some(session_id) = session_id {
            meta.insert("session_id".into(), Value::String(session_id));
        }
        if let Some(tool) = tool {
            meta.insert("tool_name".into(), Value::String(tool));
        }
        meta.insert("capture_kind".into(), Value::String(source.to_owned()));
        let lsn = self.capture_lightweight(scope, source, content, meta).await?;
        Ok(ContextResponse { lsn: Some(lsn), ..ContextResponse::empty() })
    }

    fn spawn_capture_tool(
        &self,
        scope: &Scope,
        source: &str,
        session_id: Option<String>,
        tool: Option<String>,
        payload: Value,
    ) {
        let service = self.clone();
        let scope = scope.clone();
        let source = source.to_owned();
        tokio::spawn(async move {
            if let Err(err) = service.capture_tool(&scope, &source, session_id, tool, payload).await
            {
                tracing::debug!(err = %err, "lunaris tool capture write failed");
            }
        });
    }

    async fn capture_feedback(
        &self,
        scope: &Scope,
        session_id: Option<String>,
        injected_memory_ids: Vec<String>,
        outcome: Option<String>,
    ) -> anyhow::Result<ContextResponse> {
        let content = format!(
            "turn feedback\ninjected_memory_ids: {}\noutcome: {}",
            injected_memory_ids.join(","),
            outcome.unwrap_or_else(|| "unknown".to_string())
        );
        let mut meta = Map::new();
        if let Some(session_id) = session_id {
            meta.insert("session_id".into(), Value::String(session_id));
        }
        meta.insert(
            "injected_memory_ids".into(),
            Value::Array(injected_memory_ids.into_iter().map(Value::String).collect()),
        );
        let lsn = self.capture_lightweight(scope, "codex:turn_feedback", content, meta).await?;
        Ok(ContextResponse { lsn: Some(lsn), ..ContextResponse::empty() })
    }

    async fn trace_injection(
        &self,
        scope: &Scope,
        injection_id: &str,
        phase: &str,
        session_id: Option<&str>,
        memory_ids: Vec<String>,
    ) -> anyhow::Result<()> {
        let mut meta = Map::new();
        meta.insert("injection_id".into(), Value::String(injection_id.to_owned()));
        meta.insert("phase".into(), Value::String(phase.to_owned()));
        if let Some(session_id) = session_id {
            meta.insert("session_id".into(), Value::String(session_id.to_owned()));
        }
        meta.insert(
            "memory_ids".into(),
            Value::Array(memory_ids.iter().cloned().map(Value::String).collect()),
        );
        let content = format!(
            "memory injection {injection_id}\nphase: {phase}\nmemory_ids: {}",
            memory_ids.join(",")
        );
        self.capture_lightweight(scope, "codex:memory_injection", content, meta).await?;
        Ok(())
    }

    async fn capture_lightweight(
        &self,
        scope: &Scope,
        source: &str,
        content: String,
        metadata: Map<String, Value>,
    ) -> anyhow::Result<Lsn> {
        let storage = self.storage_for_scope(scope).await?;
        let clock = HlcClock::new(0);
        let mut episode = Episode::new(scope.clone(), source.to_owned(), content, &clock);
        episode.metadata = metadata;
        let embedder = NoopEmbedder::default();
        let receipt = lunaris_ingest::ingest_episode_with_receipt(
            storage.as_ref(),
            &embedder,
            &clock,
            episode,
        )
        .await?;
        let config = EmbedPromotionConfig::from_env();
        match embed_promotion::publish_capture_receipt(
            storage.as_ref(),
            scope,
            source,
            &receipt,
            &config,
        )
        .await
        {
            Ok(Some(offset)) => {
                tracing::debug!(offset, "lunaris embed promotion event published");
                self.ensure_embed_worker(scope, config).await;
            }
            Ok(None) => {}
            Err(err) => {
                tracing::debug!(err = %err, "lunaris embed promotion publish failed");
            }
        }
        Ok(receipt.lsn)
    }

    async fn ensure_embed_worker(&self, scope: &Scope, config: EmbedPromotionConfig) {
        if !config.enabled || !config.worker_enabled {
            return;
        }

        let key = scope.as_str().to_owned();
        let mut workers = self.embed_workers.lock().await;
        if let Some(existing) = workers.get(&key)
            && !existing.is_finished()
        {
            return;
        }

        let service = self.clone();
        let scope = scope.clone();
        let worker_key = key.clone();
        let handle = tokio::spawn(async move {
            match service.handle_for_scope(&scope).await {
                Ok(handle) => {
                    if let Err(err) = embed_promotion::run_worker(handle, scope, config).await {
                        tracing::debug!(err = %err, "lunaris embed promotion worker stopped");
                    }
                }
                Err(err) => {
                    tracing::debug!(err = %err, "lunaris embed promotion worker open failed");
                }
            }
        });
        workers.insert(worker_key, handle);
    }

    fn spawn_trace_injection(
        &self,
        scope: &Scope,
        injection_id: String,
        phase: &str,
        session_id: Option<String>,
        memory_ids: Vec<String>,
    ) {
        let service = self.clone();
        let scope = scope.clone();
        let phase = phase.to_owned();
        tokio::spawn(async move {
            if let Err(err) = service
                .trace_injection(&scope, &injection_id, &phase, session_id.as_deref(), memory_ids)
                .await
            {
                tracing::debug!(err = %err, "lunaris context injection trace write failed");
            }
        });
    }
}

impl Default for ContextService {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
struct ToolContext {
    tool: Option<String>,
    paths: Option<Vec<String>>,
}

fn resolve_scope(cwd: Option<&Path>, explicit: Option<&str>) -> anyhow::Result<Scope> {
    if let Some(scope) = explicit
        && !scope.is_empty()
    {
        return Ok(Scope::new(scope)?);
    }
    let cwd_buf = match cwd {
        Some(cwd) => cwd.to_path_buf(),
        None => std::env::current_dir()?,
    };
    // Env-IGNORING: a long-lived daemon inherits LUNARIS_HOOK_SCOPE at birth;
    // honoring it here stamps that scope onto every project's unpinned request
    // (P0 cross-project bleed, 2026-07-14). Explicit request scope is handled
    // above; unpinned requests derive purely from cwd.
    Ok(crate::scope::resolve_no_env(&cwd_buf)?)
}

pub fn default_socket_path() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("LUNARIS_CONTEXTD_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no $HOME directory found"))?;
    Ok(home.join(".lunaris").join("codex-contextd.sock"))
}

fn render_context(
    phase: &str,
    tool_context: Option<&ToolContext>,
    memories: &[ContextMemory],
    max_chars: usize,
) -> String {
    let mut out = String::new();
    match (phase, tool_context) {
        ("post_tool", Some(ctx)) => {
            out.push_str("<lunaris_memory_context phase=\"post_tool\"");
            if let Some(tool) = &ctx.tool {
                out.push_str(" tool=\"");
                out.push_str(&xml_attr(tool));
                out.push('"');
            }
            out.push_str(">\nTool result may relate to these memories.\n\n");
            if let Some(paths) = &ctx.paths
                && !paths.is_empty()
            {
                out.push_str("paths: ");
                out.push_str(&paths.join(", "));
                out.push_str("\n\n");
            }
        }
        _ => {
            out.push_str("<lunaris_memory_context phase=\"prompt\">\n");
            out.push_str("Retrieved memories for this prompt. Use only when relevant.\n\n");
        }
    }

    for memory in memories {
        out.push_str("- [source=");
        out.push_str(&memory.source);
        out.push_str(" score=");
        out.push_str(&format!("{:.2}", memory.score));
        out.push_str(" id=");
        out.push_str(&memory.episode_id);
        out.push_str("] ");
        out.push_str(&single_line(&memory.snippet));
        out.push('\n');
    }
    out.push_str("</lunaris_memory_context>");
    trim_to_chars(&out, max_chars)
}

fn curate_context_memories(candidates: Vec<ContextMemory>, max_hits: usize) -> Vec<ContextMemory> {
    let mut enriched: Vec<(i32, ContextMemory)> = candidates
        .into_iter()
        .filter_map(|mut memory| {
            if excluded_context_source(&memory.source) {
                return None;
            }
            let summary = summarize_memory_for_context(&memory.source, &memory.snippet)?;
            memory.snippet = trim_to_chars(&summary, 260);
            Some((source_priority(&memory.source), memory))
        })
        .collect();

    enriched.sort_by(|(left_priority, left), (right_priority, right)| {
        right_priority
            .cmp(left_priority)
            .then_with(|| right.score.total_cmp(&left.score))
            .then_with(|| left.episode_id.cmp(&right.episode_id))
    });

    let mut seen = HashSet::new();
    let mut curated = Vec::new();
    for (_, memory) in enriched {
        let key = dedupe_key(&memory.source, &memory.snippet);
        if seen.insert(key) {
            curated.push(memory);
        }
        if curated.len() >= max_hits {
            break;
        }
    }
    curated
}

fn curate_context_memories_lossy(
    candidates: Vec<ContextMemory>,
    max_hits: usize,
) -> Vec<ContextMemory> {
    let mut enriched: Vec<(i32, ContextMemory)> = candidates
        .into_iter()
        .filter_map(|mut memory| {
            if excluded_context_source(&memory.source) {
                return None;
            }
            let summary = summarize_memory_for_context(&memory.source, &memory.snippet)
                .unwrap_or_else(|| single_line(&memory.snippet));
            if summary.trim().is_empty() {
                return None;
            }
            memory.snippet = trim_to_chars(&summary, 260);
            Some((source_priority(&memory.source), memory))
        })
        .collect();

    enriched.sort_by(|(left_priority, left), (right_priority, right)| {
        right_priority
            .cmp(left_priority)
            .then_with(|| right.score.total_cmp(&left.score))
            .then_with(|| left.episode_id.cmp(&right.episode_id))
    });

    let mut seen = HashSet::new();
    let mut curated = Vec::new();
    for (_, memory) in enriched {
        let key = dedupe_key(&memory.source, &memory.snippet);
        if seen.insert(key) {
            curated.push(memory);
        }
        if curated.len() >= max_hits {
            break;
        }
    }
    curated
}

fn excluded_context_source(source: &str) -> bool {
    matches!(
        source,
        "codex:memory_injection"
            | "codex:turn_feedback"
            | "claude-code:session_start"
            | "claude-code:stop"
    )
}

fn source_priority(source: &str) -> i32 {
    if source.starts_with("decision:") {
        90
    } else if source.starts_with("edit:") {
        85
    } else if source == "codex:tool_call:post" {
        75
    } else if source == "claude-code:post_tool_use" {
        70
    } else if source == "codex:tool_call:pre" {
        55
    } else if source == "claude-code:pre_tool_use" {
        45
    } else {
        50
    }
}

// JSON-envelope summarization lives in `lunaris_core::snippet` (shared with
// `lunaris-mcp` memory.recall — RC-1 precedent: cross-crate helpers belong in
// core). Only the low-value-text drop policy stays hook-side: recall must
// never lose a hit to it, but context injection budget-drops noise.
fn summarize_memory_for_context(source: &str, text: &str) -> Option<String> {
    if let Some(value) = parse_jsonish(text) {
        return summarize_json(source, &value);
    }
    let text = single_line(text);
    if text.is_empty() || is_low_value_text(source, &text) { None } else { Some(text) }
}

fn is_low_value_text(source: &str, text: &str) -> bool {
    source == "claude-code:pre_tool_use"
        && text.contains("file_path")
        && !text.contains("new_string")
        && !text.contains("old_string")
        && !text.contains("command")
        && !text.contains("note")
        && !text.contains("output")
}

fn dedupe_key(source: &str, snippet: &str) -> String {
    let source_class = if source.starts_with("decision:") {
        "decision"
    } else if source.starts_with("edit:") {
        "edit"
    } else {
        source
    };
    format!("{source_class}:{}", single_line(snippet).to_lowercase())
}

fn stable_query_key(text: &str) -> String {
    blake3::hash(single_line(text).trim().as_bytes()).to_hex().to_string()
}

fn summarize_json_payload(payload: &Value, max_chars: usize) -> String {
    let mut text = serde_json::to_string(payload).unwrap_or_default();
    ScrubEngine::from_default_policy().apply(&mut text);
    trim_to_chars(&text, max_chars)
}

fn scrub_and_trim(text: &str, max_chars: usize) -> String {
    let mut text = text.to_owned();
    ScrubEngine::from_default_policy().apply(&mut text);
    trim_to_chars(&text, max_chars)
}

fn xml_attr(text: &str) -> String {
    text.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;")
}

fn ulid_bytes_to_string(bytes: &[u8]) -> String {
    <[u8; 16]>::try_from(bytes)
        .map(|arr| ulid::Ulid::from_bytes(arr).to_string())
        .unwrap_or_default()
}

fn env_usize_any(names: &[&str]) -> Option<usize> {
    names.iter().find_map(|name| std::env::var(name).ok()?.parse::<usize>().ok())
}

fn env_f32_any(names: &[&str]) -> Option<f32> {
    names.iter().find_map(|name| std::env::var(name).ok()?.parse::<f32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P0 regression (2026-07-14): the daemon request path must resolve an
    /// unpinned request's scope from its `cwd`, NOT from the daemon's own
    /// birth-time `LUNARIS_HOOK_SCOPE` env. A long-lived contextd born under
    /// `cc-hook-e2e` was stamping that scope onto every project's captures.
    ///
    /// The crate is `#![forbid(unsafe_code)]`, so the env override (which is
    /// `unsafe` to mutate in Rust 2024) can't be exercised at runtime here.
    /// Instead this pins the fix at the source: `resolve_scope` MUST derive
    /// unpinned scopes via the env-ignoring `scope::resolve_no_env`, never the
    /// env-reading `scope::resolve`. The behavioral half (override=None derives
    /// from cwd) is covered by `resolve_with_path_none_override_derives_from_cwd`.
    #[test]
    fn resolve_scope_daemon_path_uses_env_ignoring_resolver() {
        let src = include_str!("context.rs");
        let body = src
            .split("fn resolve_scope(")
            .nth(1)
            .expect("resolve_scope must exist")
            .split("\nfn ")
            .next()
            .unwrap();
        assert!(
            body.contains("scope::resolve_no_env("),
            "resolve_scope must derive unpinned scopes via scope::resolve_no_env"
        );
        assert!(
            !body.contains("scope::resolve(&cwd_buf)"),
            "resolve_scope must NOT use the env-reading scope::resolve for daemon requests"
        );
    }

    /// Mechanism: with no override the scope is derived from cwd; an explicit
    /// override wins. `resolve_no_env` is exactly `override_ == None`.
    #[test]
    fn resolve_with_path_none_override_derives_from_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let scopes = tmp.path().join("scopes.json");
        let derived =
            crate::scope::resolve_with_path(tmp.path(), &scopes, None).expect("derive from cwd");
        assert!(
            derived.as_str().starts_with("cwd_") || derived.as_str().starts_with("git_"),
            "override=None must derive from cwd, got {}",
            derived.as_str()
        );
        let pinned =
            crate::scope::resolve_with_path(tmp.path(), &scopes, Some("proj-pin")).unwrap();
        assert_eq!(pinned.as_str(), "proj-pin", "an explicit override still wins");
        assert_ne!(derived.as_str(), pinned.as_str());
    }

    /// An EXPLICIT request scope is still honored verbatim (per-project pin).
    #[test]
    fn resolve_scope_honors_explicit_request_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let scope = resolve_scope(Some(tmp.path()), Some("proj-explicit")).unwrap();
        assert_eq!(scope.as_str(), "proj-explicit");
    }

    #[test]
    fn render_prompt_context_is_bounded_and_excludes_raw_layout_noise() {
        let memories = vec![ContextMemory {
            episode_id: "01HX0000000000000000000000".into(),
            source: "decision:test".into(),
            score: 0.84,
            snippet: "line one\nline two".into(),
        }];

        let rendered = render_context("prompt", None, &memories, 220);

        assert!(rendered.contains("phase=\"prompt\""));
        assert!(rendered.contains("line one line two"));
        assert!(rendered.len() <= 220);
    }

    #[test]
    fn render_post_tool_context_marks_tool_phase() {
        let memories = vec![ContextMemory {
            episode_id: "01HX0000000000000000000000".into(),
            source: "edit:test".into(),
            score: 0.71,
            snippet: "prior edit".into(),
        }];
        let ctx = ToolContext { tool: Some("read".into()), paths: Some(vec!["src/lib.rs".into()]) };

        let rendered = render_context("post_tool", Some(&ctx), &memories, 500);

        assert!(rendered.contains("phase=\"post_tool\""));
        assert!(rendered.contains("tool=\"read\""));
        assert!(rendered.contains("src/lib.rs"));
    }

    #[test]
    fn curation_summarizes_rich_hook_payloads_and_drops_file_only_noise() {
        let memories = vec![
            ContextMemory {
                episode_id: "01HX0000000000000000000001".into(),
                source: "claude-code:pre_tool_use".into(),
                score: 0.99,
                snippet: r#"{"file_path":"README.md"}"#.into(),
            },
            ContextMemory {
                episode_id: "01HX0000000000000000000002".into(),
                source: "claude-code:pre_tool_use".into(),
                score: 0.80,
                snippet: r#"{"file_path":"scripts/setup-lunaris-agents.py","old_string":"sqlite default","new_string":"Moon storage shared"}"#.into(),
            },
        ];

        let curated = curate_context_memories(memories, 5);

        assert_eq!(curated.len(), 1);
        assert!(curated[0].snippet.contains("edit scripts/setup-lunaris-agents.py"));
        assert!(curated[0].snippet.contains("Moon storage shared"));
        assert!(!curated[0].snippet.contains('{'));
        assert!(!curated[0].snippet.contains("README.md"));
    }

    #[test]
    fn curation_tolerates_scrubbed_smart_quote_json() {
        let memories = vec![ContextMemory {
            episode_id: "01HX0000000000000000000004".into(),
            source: "claude-code:post_tool_use".into(),
            score: 0.80,
            snippet: "{ “ output ” : “ Moon storage shared ” , “ success ” :true}".into(),
        }];

        let curated = curate_context_memories(memories, 5);

        assert_eq!(curated.len(), 1);
        assert_eq!(curated[0].snippet, "tool output: Moon storage shared");
    }

    #[test]
    fn curation_resolves_nested_smart_quote_tool_response() {
        // Space-padded keys survive the smart-quote reparse — the NESTED
        // object lookups must be as trim-tolerant as string_field, or the
        // whole payload summarizes to None (2026-07-14 deep-test bug; the
        // verify envelope's top-level `output` copy was the workaround).
        let memories = vec![ContextMemory {
            episode_id: "01HX0000000000000000000006".into(),
            source: "claude-code:post_tool_use".into(),
            score: 0.80,
            snippet: "{ “ tool_response ” : { “ output ” : “ Moon relay ok ” } }".into(),
        }];

        let curated = curate_context_memories(memories, 5);

        assert_eq!(curated.len(), 1);
        assert_eq!(curated[0].snippet, "tool output: Moon relay ok");
    }

    #[test]
    fn curation_summarizes_prompt_capture_envelope() {
        // Live repro 2026-07-14 (full Claude Code test): a captured
        // UserPromptSubmit envelope was the TOP recall hit but rendered as raw
        // sanitized JSON, truncating before the payload — prompt events need
        // their own summarize branch.
        let memories = vec![ContextMemory {
            episode_id: "01HX0000000000000000000007".into(),
            source: "claude-code:pre_tool_use".into(),
            score: 0.80,
            snippet: "{ “ codex_hook_event_name ” : “ UserPromptSubmit ” , “ codex_payload ” :{ “ cwd ” : “ /tmp ” , “ hook_event_name ” : “ UserPromptSubmit ” , “ prompt ” : “ the crimson beacon marker is XR-9913 on port 5252 ” } }".into(),
        }];

        let curated = curate_context_memories(memories, 5);

        assert_eq!(curated.len(), 1);
        assert!(
            curated[0].snippet.starts_with("prompt: "),
            "prompt envelope must summarize, got: {:?}",
            curated[0].snippet
        );
        assert!(curated[0].snippet.contains("XR-9913"), "payload must survive the snippet cap");
        assert!(!curated[0].snippet.contains('{'), "no raw JSON in the snippet");
    }

    #[test]
    fn curation_tool_output_wins_over_prompt() {
        let memories = vec![ContextMemory {
            episode_id: "01HX0000000000000000000008".into(),
            source: "claude-code:post_tool_use".into(),
            score: 0.80,
            snippet: r#"{"output": "deploy ok", "prompt": "ignore me"}"#.into(),
        }];

        let curated = curate_context_memories(memories, 5);

        assert_eq!(curated.len(), 1);
        assert_eq!(curated[0].snippet, "tool output: deploy ok");
    }

    #[test]
    fn inject_snippet_scrubs_stored_api_key() {
        // Defense-in-depth: a secret that reached storage before the scrubber
        // knew its shape must still be redacted on the way into
        // additionalContext.
        let rendered = scrub_and_trim("the key is sk-ant-api03-AbCd1234EfGh5678IjKl", 900);
        assert!(
            rendered.contains("<REDACTED:API_KEY>"),
            "inject-side scrub must redact stored API keys, got: {rendered:?}"
        );
        assert!(!rendered.contains("sk-ant-"), "raw key must never render, got: {rendered:?}");
    }

    #[test]
    fn curation_excludes_injection_traces() {
        let memories = vec![ContextMemory {
            episode_id: "01HX0000000000000000000003".into(),
            source: "codex:memory_injection".into(),
            score: 1.0,
            snippet: "memory injection".into(),
        }];

        assert!(curate_context_memories(memories, 5).is_empty());
    }
}

//! Content-addressed extraction cache — [`CachedExtractor`] decorator.
//!
//! LLM extraction dominates graph-ingest wall clock (a MiniMax-M3 call runs
//! ~23s; ~80% of graph-ON LongMemEval time). This decorator wraps any
//! [`Extractor`] and guarantees each distinct (prompt-template, namespace,
//! chunk) triple hits the LLM at most once; afterwards the chunk's
//! [`RawExtraction`] replays from a filesystem cache at filesystem speed.
//!
//! ## Key derivation
//!
//! `key = blake3(namespace || 0x00 || build_prompt(chunk))`
//!
//! [`crate::llm_extractor::build_prompt`] is the single prompt source shared
//! by `LlmExtractor` and `CloudApiExtractor`, and it embeds the chunk text,
//! the heading path AND the template itself — so editing the template
//! auto-invalidates every stale entry with no manual version bump. The
//! namespace carries model identity (extraction differs per model).
//!
//! ## Replay semantics
//!
//! Chunk ids are fresh Ulids on every ingest, so a replayed entry's
//! `source_chunk_id` is rewritten to the CURRENT chunk id — provenance
//! follows the live ingest, not the fill pass. Everything else (entity ids,
//! fact ids, timestamps) replays byte-identically, which makes re-ingest
//! deterministic — a property the live extractor cannot offer (the prompt's
//! "else today" date instruction and sampling drift both disappear).
//!
//! ## Failure design
//!
//! The cache is an accelerator, never a dependency:
//! - a corrupt/unreadable entry is a MISS (re-extracted and healed), not an
//!   error;
//! - a failed write is a `tracing::warn`, extraction still succeeds;
//! - entries are written to a unique tmp file then `rename`d, so concurrent
//!   fill processes on the same directory never observe torn JSON.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use lunaris_core::LunarisError;
use ulid::Ulid;

use crate::Extractor;
use crate::llm_extractor::build_prompt;
use crate::types::{ChunkInput, RawExtraction, RawExtractionBatch};

/// Hit/miss counters for observability — the LME harness prints these after
/// ingest so a mis-wired cache (0 hits on a warm dir) is visible immediately.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
}

/// Filesystem-backed content-addressed cache around any [`Extractor`].
pub struct CachedExtractor {
    inner: Arc<dyn Extractor>,
    dir: PathBuf,
    namespace: String,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl std::fmt::Debug for CachedExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedExtractor")
            .field("dir", &self.dir)
            .field("namespace", &self.namespace)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl CachedExtractor {
    /// Wrap `inner`, persisting entries under `dir` (created if absent).
    ///
    /// `namespace` must identify the model producing extractions (e.g.
    /// `"MiniMax-M3"`) — entries never replay across namespaces.
    pub fn new(
        inner: Arc<dyn Extractor>,
        dir: impl AsRef<Path>,
        namespace: &str,
    ) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            inner,
            dir,
            namespace: namespace.to_owned(),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }

    /// Per-chunk hit/miss counts since construction.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
        }
    }

    fn entry_path(&self, chunk: &ChunkInput) -> PathBuf {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.namespace.as_bytes());
        hasher.update(&[0u8]);
        hasher.update(build_prompt(chunk).as_bytes());
        self.dir.join(format!("{}.json", hasher.finalize().to_hex()))
    }

    /// Read + parse one entry. ANY failure (absent, unreadable, corrupt,
    /// schema drift) is `None` — the caller re-extracts and heals the entry.
    fn read_entry(&self, chunk: &ChunkInput) -> Option<RawExtraction> {
        let bytes = std::fs::read(self.entry_path(chunk)).ok()?;
        serde_json::from_slice::<RawExtraction>(&bytes).ok()
    }

    /// Best-effort atomic write: unique tmp file in the same directory, then
    /// rename. Concurrent fill processes race benignly (last rename wins;
    /// every candidate holds identical-shape valid JSON).
    fn write_entry(&self, chunk: &ChunkInput, raw: &RawExtraction) {
        let path = self.entry_path(chunk);
        let tmp = self.dir.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            Ulid::new()
        ));
        let result = serde_json::to_vec(raw)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&tmp, bytes))
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            tracing::warn!(err = %e, path = %path.display(), "extraction cache write failed; continuing uncached");
        }
    }
}

#[async_trait]
impl Extractor for CachedExtractor {
    async fn extract(
        &self,
        episode_id: Ulid,
        chunks: &[ChunkInput],
    ) -> Result<RawExtractionBatch, LunarisError> {
        if chunks.is_empty() {
            return Ok(RawExtractionBatch::default());
        }

        // Probe the cache per chunk; replayed entries adopt the CURRENT
        // chunk id (fresh per ingest — provenance must follow it).
        let mut out: Vec<Option<RawExtraction>> = vec![None; chunks.len()];
        let mut miss_idx: Vec<usize> = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            match self.read_entry(chunk) {
                Some(mut raw) => {
                    raw.source_chunk_id = chunk.chunk_id;
                    out[i] = Some(raw);
                }
                None => miss_idx.push(i),
            }
        }
        self.hits.fetch_add((chunks.len() - miss_idx.len()) as u64, Ordering::Relaxed);
        self.misses.fetch_add(miss_idx.len() as u64, Ordering::Relaxed);

        // Misses go to the inner extractor as ONE batch (its per-batch
        // timeout / fallback semantics stay intact), aligned by index per
        // the Extractor contract (one RawExtraction per input chunk, in
        // input order).
        if !miss_idx.is_empty() {
            let miss_chunks: Vec<ChunkInput> =
                miss_idx.iter().map(|&i| chunks[i].clone()).collect();
            let batch = self.inner.extract(episode_id, &miss_chunks).await?;
            for (j, &i) in miss_idx.iter().enumerate() {
                let raw = batch.by_chunk.get(j).cloned().unwrap_or_else(|| RawExtraction {
                    source_chunk_id: chunks[i].chunk_id,
                    ..Default::default()
                });
                self.write_entry(&chunks[i], &raw);
                out[i] = Some(raw);
            }
        }

        Ok(RawExtractionBatch {
            by_chunk: out
                .into_iter()
                .enumerate()
                .map(|(i, o)| {
                    o.unwrap_or_else(|| RawExtraction {
                        source_chunk_id: chunks[i].chunk_id,
                        ..Default::default()
                    })
                })
                .collect(),
        })
    }

    fn applies(&self) -> bool {
        self.inner.applies()
    }
}

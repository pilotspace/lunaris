//! `memory.ingest` — ingest an observation into agent memory.
//!
//! **INGEST-04 invariant**: this handler MUST call `ScopedLunaris::ingest`
//! (or `ScopedLunaris::ingest_idempotent` for dedupe paths) and NEVER call
//! `atomic_write` directly. `grep -c 'atomic_write'
//! crates/lunaris-mcp/src/tools/ingest.rs` must return 0.
//!
//! The scope comes from `AppState::scope`, which is bound at server startup.
//! Wire payloads cannot supply or override the scope — CLAUDE.md DTO discipline.

use chrono::{DateTime, Utc};
use lunaris::{EpisodeBuilder, IngestKind};
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::tools::ToolError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.ingest`.
///
/// `#[serde(deny_unknown_fields)]` is mandatory (CLAUDE.md §HTTP DTO discipline).
/// The scope field is absent by design — it is bound at server startup.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct IngestParams {
    /// Logical origin of the observation (e.g. "helios/task-planner", "user").
    pub source: String,

    /// Raw text content of the observation.
    pub content: String,

    /// Optional reference time in RFC-3339 format.
    ///
    /// When absent the HLC wall clock at ingest time is used.
    #[serde(default)]
    pub t_ref: Option<String>,

    /// Arbitrary key-value metadata forwarded to the episode.
    #[serde(default)]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,

    /// Optional dedupe key (HOOK-05). If present and already seen in this scope,
    /// returns the prior LSN without a second `atomic_write`. Callers can supply
    /// a pre-computed blake3 hex string (e.g. from `lunaris-hook`) or any opaque
    /// string — the storage layer treats it as an opaque key scoped by `(scope, dedupe_key)`.
    ///
    /// When absent (the common case), the normal `ingest` path is taken.
    #[serde(default)]
    pub dedupe_key: Option<String>,
}

/// Output of a successful `memory.ingest` call.
///
/// `lsn` is the log-sequence number of the committed write, formatted as
/// `"{wall_ms}:{counter}"`. Callers may use it as an opaque ordering handle.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct IngestResponse {
    /// Log-sequence number of the committed ingest (wall_ms:counter).
    pub lsn: String,

    /// True if this call returned a previously-committed LSN (dedupe hit).
    ///
    /// When `true`, no new `atomic_write` was issued — the prior LSN is returned.
    /// When `false` (or absent — default), a fresh episode was written.
    #[serde(default)]
    pub was_duplicate: bool,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.ingest`.
///
/// 1. Parse optional `t_ref` from RFC-3339.
/// 2. Build an [`EpisodeBuilder`] with source, content, and any optional fields.
/// 3. If `dedupe_key` is present: call `ScopedLunaris::ingest_idempotent`.
///    If the key was already seen, return the prior LSN with `was_duplicate=true`.
/// 4. If `dedupe_key` is absent: delegate to `ScopedLunaris::ingest` (the single
///    `atomic_write` call — INGEST-04). Return with `was_duplicate=false`.
pub(crate) async fn handle(
    state: &AppState,
    params: IngestParams,
) -> Result<IngestResponse, ToolError> {
    // Parse t_ref before touching the engine — fail fast on bad input.
    let t_ref: Option<DateTime<Utc>> = params
        .t_ref
        .as_deref()
        .map(|s| {
            DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&Utc)).map_err(|e| {
                ToolError::InvalidInput(format!("t_ref is not a valid RFC-3339 timestamp: {e}"))
            })
        })
        .transpose()?;

    // Build the episode. Only chain optional methods when values are present.
    let mut builder = EpisodeBuilder::new(params.source, params.content);
    if let Some(t) = t_ref {
        builder = builder.t_ref(t);
    }
    if let Some(meta) = params.metadata {
        if !meta.is_empty() {
            builder = builder.metadata(meta);
        }
    }

    // Re-derive ScopedLunaris per call — never store it in AppState.
    let scoped = state.lunaris.scoped(state.scope.clone());

    if let Some(ref key) = params.dedupe_key {
        // HOOK-05 idempotent path: check dedupe key before writing.
        // INGEST-04: the single atomic_write lives inside ingest_idempotent -> ingest.
        let (lsn, kind) = scoped.ingest_idempotent(builder, key).await?;
        let was_duplicate = matches!(kind, IngestKind::Duplicate(_));

        tracing::debug!(
            scope    = state.scope.as_str(),
            lsn      = %lsn,
            dedupe   = %key,
            duplicate = was_duplicate,
            "memory.ingest committed (idempotent path)",
        );

        Ok(IngestResponse { lsn: lsn.to_string(), was_duplicate })
    } else {
        // Standard path: no dedupe key supplied — always write a fresh episode.
        // INGEST-04: the single atomic_write lives inside ScopedLunaris::ingest.
        let lsn = scoped.ingest(builder).await?;

        tracing::debug!(
            scope = state.scope.as_str(),
            lsn   = %lsn,
            "memory.ingest committed",
        );

        Ok(IngestResponse { lsn: lsn.to_string(), was_duplicate: false })
    }
}

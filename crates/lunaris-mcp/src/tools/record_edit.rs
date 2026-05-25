//! `memory.record_edit` — record a file edit into agent memory.
//!
//! INGEST-04 invariant: this handler MUST call `ScopedLunaris::ingest`
//! (or `ScopedLunaris::ingest_idempotent`) and NEVER call `atomic_write` directly.
//! `grep -c 'atomic_write' crates/lunaris-mcp/src/tools/record_edit.rs` must return 0.
//!
//! The scope comes from `AppState::scope`, which is bound at server startup.
//! Wire payloads cannot supply or override the scope — CLAUDE.md DTO discipline.

use lunaris::{EpisodeBuilder, IngestKind};
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::tools::ToolError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.record_edit`.
///
/// `#[serde(deny_unknown_fields)]` is mandatory (CLAUDE.md §HTTP DTO discipline).
/// The scope field is absent by design — it is bound at server startup and cannot
/// be overridden by the wire payload (T-25-02-01 threat mitigation).
///
/// Security note (T-25-02-03): `before`, `after`, and `intent` are user-controlled
/// strings stored as-is in the content body. Downstream callers rendering these values
/// MUST treat them as untrusted (could contain secrets if the caller passes them without
/// scrubbing). The `deny_unknown_fields` attribute blocks payload-smuggling (T-25-02-01).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordEditParams {
    /// The file path that was edited (stored in metadata for filter-by-path queries).
    pub path: String,

    /// Optional content before the edit (for diff context).
    #[serde(default)]
    pub before: Option<String>,

    /// Content after the edit.
    pub after: String,

    /// Optional intent / rationale for the edit.
    #[serde(default)]
    pub intent: Option<String>,

    /// Optional dedupe key (HOOK-05). If present and already seen in this scope,
    /// returns the prior LSN without a second write. Callers can supply any opaque
    /// string — the storage layer scopes it by `(scope, dedupe_key)`.
    ///
    /// When absent (the common case), the normal `ingest` path is taken.
    #[serde(default)]
    pub dedupe_key: Option<String>,
}

/// Output of a successful `memory.record_edit` call.
///
/// `lsn` is the log-sequence number of the committed write, formatted as
/// `"{wall_ms}:{counter}"`. Callers may use it as an opaque ordering handle.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct RecordEditResponse {
    /// Log-sequence number of the committed write (wall_ms:counter).
    pub lsn: String,

    /// True if this call returned a previously-committed LSN (dedupe hit).
    ///
    /// When `true`, no new write was issued — the prior LSN is returned.
    /// When `false` (or absent — default), a fresh episode was written.
    #[serde(default)]
    pub was_duplicate: bool,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.record_edit`.
///
/// 1. Compute `source = "edit:<scope>"` from the server-bound scope.
/// 2. Serialize content as a JSON body containing `path`, `before`, `after`,
///    and `intent` (all fields except `dedupe_key`).
/// 3. Build metadata `{"kind": "edit", "path": "<params.path>"}`.
///    The `path` field in metadata enables future `memory.recall` filter-by-path
///    queries via `Filter::Eq("path", ...)`.
/// 4. If `dedupe_key` is present: call `ScopedLunaris::ingest_idempotent`.
/// 5. If `dedupe_key` is absent: call `ScopedLunaris::ingest`.
pub(crate) async fn handle(
    state: &AppState,
    params: RecordEditParams,
) -> Result<RecordEditResponse, ToolError> {
    let source = format!("edit:{}", state.scope.as_str());

    // Serialize content as structured JSON of the edit payload.
    // All fields except `dedupe_key` — that is a transport concern, not memory content.
    #[derive(Serialize)]
    struct EditPayload<'a> {
        path: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        before: Option<&'a str>,
        after: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        intent: Option<&'a str>,
    }
    let payload = EditPayload {
        path: &params.path,
        before: params.before.as_deref(),
        after: &params.after,
        intent: params.intent.as_deref(),
    };
    let content = serde_json::to_string(&payload)
        .map_err(|e| ToolError::InvalidInput(format!("serialize edit payload: {e}")))?;

    // Metadata: `{"kind": "edit", "path": "<params.path>"}`.
    // The `path` field here (not just in content) enables future Filter::Eq("path", ...) queries.
    let mut meta = serde_json::Map::new();
    meta.insert("kind".into(), serde_json::Value::String("edit".into()));
    meta.insert("path".into(), serde_json::Value::String(params.path.clone()));

    let mut builder = EpisodeBuilder::new(source, content);
    builder = builder.metadata(meta);

    // Re-derive ScopedLunaris per call — never store it in AppState.
    let scoped = state.lunaris.scoped(state.scope.clone());

    if let Some(ref key) = params.dedupe_key {
        // HOOK-05 idempotent path: check dedupe key before writing.
        let (lsn, kind) = scoped.ingest_idempotent(builder, key).await?;
        let was_duplicate = matches!(kind, IngestKind::Duplicate(_));

        tracing::debug!(
            scope     = state.scope.as_str(),
            lsn       = %lsn,
            path      = %params.path,
            dedupe    = %key,
            duplicate = was_duplicate,
            "memory.record_edit committed (idempotent path)",
        );

        Ok(RecordEditResponse { lsn: lsn.to_string(), was_duplicate })
    } else {
        // Standard path: no dedupe key supplied — always write a fresh episode.
        let lsn = scoped.ingest(builder).await?;

        tracing::debug!(
            scope = state.scope.as_str(),
            lsn   = %lsn,
            path  = %params.path,
            "memory.record_edit committed",
        );

        Ok(RecordEditResponse { lsn: lsn.to_string(), was_duplicate: false })
    }
}

//! `memory.record_decision` — record an architectural decision into agent memory.
//!
//! INGEST-04 invariant: this handler MUST call `ScopedLunaris::ingest`
//! (or `ScopedLunaris::ingest_idempotent`) and NEVER call `atomic_write` directly.
//! `grep -c 'atomic_write' crates/lunaris-mcp/src/tools/record_decision.rs` must return 0.
//!
//! The scope comes from `AppState::scope`, which is bound at server startup.
//! Wire payloads cannot supply or override the scope — CLAUDE.md DTO discipline.

use lunaris::{EpisodeBuilder, IngestKind};
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::tools::ToolError;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Input parameters for `memory.record_decision`.
///
/// `#[serde(deny_unknown_fields)]` is mandatory (CLAUDE.md §HTTP DTO discipline).
/// The scope field is absent by design — it is bound at server startup and cannot
/// be overridden by the wire payload (T-25-01-01 threat mitigation).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordDecisionParams {
    /// The decision that was made.
    pub decision: String,

    /// The rationale behind the decision.
    pub rationale: String,

    /// Optional list of alternatives that were considered.
    #[serde(default)]
    pub alternatives: Option<Vec<String>>,

    /// Optional tags for categorisation (e.g. `["arch", "storage"]`).
    #[serde(default)]
    pub tags: Option<Vec<String>>,

    /// Optional dedupe key (HOOK-05). If present and already seen in this scope,
    /// returns the prior LSN without a second write. Callers can supply any opaque
    /// string — the storage layer scopes it by `(scope, dedupe_key)`.
    ///
    /// When absent (the common case), the normal `ingest` path is taken.
    #[serde(default)]
    pub dedupe_key: Option<String>,
}

/// Output of a successful `memory.record_decision` call.
///
/// `lsn` is the log-sequence number of the committed write, formatted as
/// `"{wall_ms}:{counter}"`. Callers may use it as an opaque ordering handle.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub(crate) struct RecordDecisionResponse {
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

/// Execute `memory.record_decision`.
///
/// 1. Compute `source = "decision:<scope>"` from the server-bound scope.
/// 2. Serialize content as a JSON body containing `decision`, `rationale`,
///    `alternatives`, and `tags` (all fields except `dedupe_key`).
/// 3. Build metadata `{"kind": "decision", "tag_count": N}`.
/// 4. If `dedupe_key` is present: call `ScopedLunaris::ingest_idempotent`.
/// 5. If `dedupe_key` is absent: call `ScopedLunaris::ingest`.
///
/// Security note (T-25-01-02): `tags` and `rationale` are user-controlled strings
/// stored as-is in the content body. Downstream callers rendering these values
/// MUST treat them as untrusted. The `deny_unknown_fields` attribute blocks
/// payload-smuggling (T-25-01-01).
pub(crate) async fn handle(
    state: &AppState,
    params: RecordDecisionParams,
) -> Result<RecordDecisionResponse, ToolError> {
    let source = format!("decision:{}", state.scope.as_str());
    let tag_count = params.tags.as_ref().map_or(0, |v| v.len());

    // Serialize content as structured JSON of the decision payload.
    // All fields except `dedupe_key` — that is a transport concern, not memory content.
    #[derive(Serialize)]
    struct DecisionPayload<'a> {
        decision: &'a str,
        rationale: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        alternatives: Option<&'a Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tags: Option<&'a Vec<String>>,
    }
    let payload = DecisionPayload {
        decision: &params.decision,
        rationale: &params.rationale,
        alternatives: params.alternatives.as_ref(),
        tags: params.tags.as_ref(),
    };
    let content = serde_json::to_string(&payload)
        .map_err(|e| ToolError::InvalidInput(format!("serialize decision payload: {e}")))?;

    let mut meta = serde_json::Map::new();
    meta.insert("kind".into(), serde_json::Value::String("decision".into()));
    meta.insert("tag_count".into(), serde_json::Value::Number(tag_count.into()));

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
            dedupe    = %key,
            duplicate = was_duplicate,
            "memory.record_decision committed (idempotent path)",
        );

        Ok(RecordDecisionResponse { lsn: lsn.to_string(), was_duplicate })
    } else {
        // Standard path: no dedupe key supplied — always write a fresh episode.
        let lsn = scoped.ingest(builder).await?;

        tracing::debug!(
            scope = state.scope.as_str(),
            lsn   = %lsn,
            "memory.record_decision committed",
        );

        Ok(RecordDecisionResponse { lsn: lsn.to_string(), was_duplicate: false })
    }
}

//! `lunaris-hook` library — hook envelope processing.
//!
//! `main.rs` is thin: it reads stdin, calls `run`, and maps the result to
//! a sysexits.h exit code. All testable logic lives here.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

pub mod citation;
pub mod context;
pub mod dedupe;
pub(crate) mod embed_promotion;
pub mod envelope;
pub mod filter;
pub(crate) mod git_anchor;
pub mod handover;
pub mod ingest;
pub mod policy;
pub mod scope;
pub mod scrub;
pub mod session_marker;
pub(crate) mod staleness;
pub mod transcript;
pub mod watchdog;

use std::sync::Arc;

use lunaris::{EpisodeBuilder, IngestKind, Lunaris};
use lunaris_core::{Episode, HlcClock, Lsn, NoopEmbedder, Scope, StoragePort};

/// Session-switch detection (session-switch-detect task).
///
/// SessionStart: record the new session in the durable marker and report a
/// switch when a DIFFERENT session was active (crash-safe — needs no
/// SessionEnd). SessionEnd: mark the session cleanly ended. All marker IO
/// degrades to warn + continue (HOOK-06: never changes exit codes, never
/// blocks the agent). Runs BEFORE the filter so the marker tracks reality
/// even when an event is policy-denied.
fn observe_session_marker(
    event: &envelope::HookEvent,
    scope: &Scope,
) -> Option<session_marker::SwitchObserved> {
    match event {
        envelope::HookEvent::SessionStart(p) => {
            let path = session_marker::sessions_file_path();
            let switch = session_marker::observe_start_at(&path, scope.as_str(), &p.session_id);
            if let Some(sw) = &switch {
                session_marker::log_switch_line(scope.as_str(), sw, &p.session_id);
            }
            switch
        }
        envelope::HookEvent::SessionEnd(p) => {
            let path = session_marker::sessions_file_path();
            session_marker::observe_end_at(&path, scope.as_str(), &p.session_id);
            None
        }
        _ => None,
    }
}

/// Errors returned by [`run`].
///
/// Note: unknown event kind is NOT an error — `run` returns `Ok(None)` for
/// unknown kinds (caller exits 0). Only actual failures produce `Err`.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    /// JSON parse failure or missing required field (exit 64).
    #[error("envelope parse error: {0}")]
    Parse(#[from] envelope::ParseError),

    /// Storage substrate rejected the write (exit 65).
    #[error("ingest error: {0}")]
    Ingest(#[from] lunaris_core::LunarisError),

    /// Event rejected by filter policy — exit 66 (no Episode written, not an error).
    #[error("event filtered by policy: {0}")]
    Filtered(String),
}

impl HookError {
    /// Map to a sysexits.h-style exit code.
    pub fn exit_code(&self) -> i32 {
        match self {
            HookError::Parse(_) => 64,
            HookError::Ingest(_) => 65,
            HookError::Filtered(_) => 66,
        }
    }
}

/// Process one hook envelope.
///
/// Full 8-step HOOK-05 pipeline (B1/B2 corrected order):
///
/// 1. Parse stdin bytes as `serde_json::Value`.
/// 2. Parse typed `HookEvent` from the `Value` (short-circuit Unknown → `Ok(None)`).
/// 3. Apply `FilterPolicy` — path/kind/size verdict (HOOK-03). Deny → `Err(Filtered)`.
/// 4. Extract kind-specific content string (`extract_content_string`).
/// 5. Truncate content if >128 KiB (`FilterPolicy::truncate_payload`).
/// 6. Scrub the truncated content for secrets (HOOK-04).
/// 7. Build `EpisodeBuilder` from SCRUBBED content (`build_episode_from_scrubbed`).
///    Splice scrubbed content back into the `Value` clone for canonical hashing.
/// 8. Compute `dedupe_key` via `canonical_json` + `derive_dedupe_key`.
///    Call `ingest_idempotent` — returns `(Lsn, IngestKind)`. Duplicate → same Lsn.
///
/// Returns:
/// - `Ok(Some(lsn))` — Episode written (Fresh) or duplicate (prior Lsn), caller exits 0.
/// - `Ok(None)` — Unknown event kind, no Episode written, caller exits 0.
/// - `Err(HookError::Parse)` — Malformed JSON or missing field, caller exits 64.
/// - `Err(HookError::Ingest)` — Storage rejected the write, caller exits 65.
/// - `Err(HookError::Filtered)` — Event rejected by filter policy, caller exits 66.
pub async fn run(
    stdin_bytes: &[u8],
    scope: Scope,
    lunaris: Arc<Lunaris>,
) -> Result<Option<Lsn>, HookError> {
    // Step 1: Parse raw bytes into a serde_json::Value.
    // We keep the Value alongside the typed event so canonical_json can hash it.
    let event_value: serde_json::Value = serde_json::from_slice(stdin_bytes)
        .map_err(|e| envelope::ParseError::InvalidJson(e.to_string()))?;

    // Step 2: Parse typed HookEvent from the Value.
    let event = envelope::parse_value(&event_value)?;

    // Unknown kinds are no-ops — exit 0 without writing anything.
    if let envelope::HookEvent::Unknown(ref kind) = event {
        tracing::info!(kind = %kind, "unknown hook event kind — no-op (exit 0)");
        return Ok(None);
    }

    // Session-switch detection — before the filter so the marker tracks
    // reality even when this event is policy-denied.
    let switch = observe_session_marker(&event, &scope);

    // Step 3: Apply FilterPolicy — path/kind verdict (HOOK-03).
    let policy =
        filter::FilterPolicy::from_env().map_err(|e| HookError::Filtered(e.to_string()))?;
    if policy.apply(&event) == filter::FilterVerdict::Deny {
        tracing::debug!(scope = scope.as_str(), "event rejected by filter policy — exit 66",);
        return Err(HookError::Filtered("event rejected by filter policy".into()));
    }

    // Step 4: Extract kind-specific content string from the envelope Value.
    // This is the raw (un-scrubbed) content field: tool_input JSON for PreToolUse,
    // tool_response JSON for PostToolUse, empty string for Stop/SessionStart.
    let raw_content = envelope::extract_content_string(&event_value);

    // Step 5: Truncate raw content if it exceeds 128 KiB.
    // Truncation runs on the raw string so head/tail coverage is maximised.
    let truncated = filter::FilterPolicy::truncate_payload(&raw_content);
    let truncated_bytes = truncated.truncated_bytes;

    // Step 6: Scrub the truncated content for secrets (HOOK-04).
    // Scrubbing runs on the TRUNCATED string — secrets in the elided middle are
    // not a concern because the elision marker replaces them.
    let scrub_engine = scrub::ScrubEngine::from_default_policy();
    let mut scrubbed_content = truncated.content;
    scrub_engine.apply(&mut scrubbed_content);

    // Step 7: Build EpisodeBuilder from SCRUBBED content.
    // This is the scrubbed builder — no un-scrubbed bytes reach storage.
    // Built via parts so an observed session switch can ride the episode
    // metadata (switch_from / switch_prev_ended) WITHOUT touching the
    // canonical envelope bytes the dedupe key hashes below.
    let mut parts =
        ingest::build_episode_parts_from_scrubbed(&event_value, &scrubbed_content, truncated_bytes)
            .map_err(|e| {
                HookError::Ingest(lunaris_core::LunarisError::Validate(
                    lunaris_core::ValidateError::Contradiction(e.to_string()),
                ))
            })?;
    if let Some(sw) = &switch {
        for (k, v) in session_marker::switch_meta(sw) {
            parts.metadata.insert(k, v);
        }
    }
    let mut builder = EpisodeBuilder::new(parts.source, parts.content);
    builder = builder.t_ref(parts.t_ref);
    if !parts.metadata.is_empty() {
        builder = builder.metadata(parts.metadata);
    }

    // Step 8: Compute dedupe key from canonical JSON of the post-scrub envelope,
    // then call ingest_idempotent (HOOK-05).
    //
    // Splice the scrubbed content back into a clone of the envelope Value so
    // canonical_json hashes the post-scrub form. This ensures two replays that
    // both redact the same secret produce the same dedupe key.
    let canonical_bytes = {
        let mut canonical_value = event_value.clone();
        envelope::splice_content_string(&mut canonical_value, &scrubbed_content);
        dedupe::canonical_json(&canonical_value)
    };

    let event_id = envelope::extract_event_id(&event).unwrap_or_else(|| {
        dedupe::derive_event_id(
            envelope::extract_session_id(&event),
            envelope::extract_kind_str(&event),
            envelope::extract_transcript_path(&event),
            envelope::extract_timestamp_str(&event),
        )
    });

    let dedupe_key = dedupe::derive_dedupe_key(scope.as_str(), &event_id, &canonical_bytes);

    let scoped = lunaris.scoped(scope);
    let (lsn, kind) =
        scoped.ingest_idempotent(builder, &dedupe_key).await.map_err(HookError::Ingest)?;

    if matches!(kind, IngestKind::Duplicate(_)) {
        tracing::debug!(
            dedupe_key = %dedupe_key,
            lsn = %lsn,
            "duplicate hook event — returning prior LSN (HOOK-05)",
        );
    }

    Ok(Some(lsn))
}

/// What one hook invocation produced (session-context-inject task).
///
/// `lsn` mirrors [`run_with_storage`]'s return; `switch` carries the
/// session-switch observation so `main.rs` can run the post-ingest
/// `additionalContext` build OUTSIDE the HOOK-06 ingest budget.
#[derive(Debug)]
pub struct HookRunOutcome {
    /// `Some(lsn)` when an Episode was written (or deduped); `None` for
    /// unknown-kind no-ops.
    pub lsn: Option<Lsn>,
    /// `Some` when this event was a SessionStart that switched sessions.
    pub switch: Option<session_marker::SwitchObserved>,
}

/// Process one hook envelope using raw storage instead of the full Lunaris
/// handle. The hook hot path only writes episodes, so this avoids constructing
/// native embedders/rerankers on every hook process invocation.
pub async fn run_with_storage(
    stdin_bytes: &[u8],
    scope: Scope,
    storage: Arc<dyn StoragePort>,
) -> Result<Option<Lsn>, HookError> {
    run_with_storage_outcome(stdin_bytes, scope, storage).await.map(|o| o.lsn)
}

/// [`run_with_storage`] variant that also reports the session-switch
/// observation, so the caller can drive the SessionStart `additionalContext`
/// injection (session-context-inject task) with its own budget.
pub async fn run_with_storage_outcome(
    stdin_bytes: &[u8],
    scope: Scope,
    storage: Arc<dyn StoragePort>,
) -> Result<HookRunOutcome, HookError> {
    let event_value: serde_json::Value = serde_json::from_slice(stdin_bytes)
        .map_err(|e| envelope::ParseError::InvalidJson(e.to_string()))?;

    let event = envelope::parse_value(&event_value)?;
    if let envelope::HookEvent::Unknown(ref kind) = event {
        tracing::info!(kind = %kind, "unknown hook event kind — no-op (exit 0)");
        return Ok(HookRunOutcome { lsn: None, switch: None });
    }

    // Session-switch detection — same placement as `run` (before the filter).
    let switch = observe_session_marker(&event, &scope);

    let policy =
        filter::FilterPolicy::from_env().map_err(|e| HookError::Filtered(e.to_string()))?;
    if policy.apply(&event) == filter::FilterVerdict::Deny {
        tracing::debug!(scope = scope.as_str(), "event rejected by filter policy — exit 66",);
        return Err(HookError::Filtered("event rejected by filter policy".into()));
    }

    let raw_content = envelope::extract_content_string(&event_value);
    let truncated = filter::FilterPolicy::truncate_payload(&raw_content);
    let truncated_bytes = truncated.truncated_bytes;

    let scrub_engine = scrub::ScrubEngine::from_default_policy();
    let mut scrubbed_content = truncated.content;
    scrub_engine.apply(&mut scrubbed_content);

    let mut parts =
        ingest::build_episode_parts_from_scrubbed(&event_value, &scrubbed_content, truncated_bytes)
            .map_err(|e| {
                HookError::Ingest(lunaris_core::LunarisError::Validate(
                    lunaris_core::ValidateError::Contradiction(e.to_string()),
                ))
            })?;
    if let Some(sw) = &switch {
        for (k, v) in session_marker::switch_meta(sw) {
            parts.metadata.insert(k, v);
        }
    }

    let canonical_bytes = {
        let mut canonical_value = event_value.clone();
        envelope::splice_content_string(&mut canonical_value, &scrubbed_content);
        dedupe::canonical_json(&canonical_value)
    };

    let event_id = envelope::extract_event_id(&event).unwrap_or_else(|| {
        dedupe::derive_event_id(
            envelope::extract_session_id(&event),
            envelope::extract_kind_str(&event),
            envelope::extract_transcript_path(&event),
            envelope::extract_timestamp_str(&event),
        )
    });
    let dedupe_key = dedupe::derive_dedupe_key(scope.as_str(), &event_id, &canonical_bytes);

    match storage.lookup_by_dedupe_key(&scope, &dedupe_key).await {
        Ok(Some(prior_lsn)) => {
            return Ok(HookRunOutcome { lsn: Some(prior_lsn), switch });
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                err = %e,
                dedupe_key,
                "dedupe key lookup failed — proceeding as fresh ingest",
            );
        }
    }

    let clock = HlcClock::new(0);
    let mut episode = Episode::new(scope.clone(), parts.source, parts.content, &clock);
    episode.t_ref = Some(parts.t_ref);
    episode.metadata = parts.metadata;

    let embedder = NoopEmbedder::default();
    let lsn = lunaris_ingest::ingest_episode(storage.as_ref(), &embedder, &clock, episode)
        .await
        .map_err(HookError::Ingest)?;

    if let Err(e) = storage.insert_dedupe_key(&scope, &dedupe_key, lsn).await {
        tracing::warn!(
            err = %e,
            dedupe_key,
            "dedupe key insert failed after hook ingest commit",
        );
    }

    Ok(HookRunOutcome { lsn: Some(lsn), switch })
}

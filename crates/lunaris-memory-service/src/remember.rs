//! `memory.remember` — the direct-capture write path (W4.3b).
//!
//! INGEST-04 invariant: this handler MUST call `ScopedLunaris::ingest`
//! (or `ScopedLunaris::ingest_idempotent`) and NEVER call `atomic_write`
//! directly. `grep -c 'atomic_write' crates/lunaris-memory-service/src/remember.rs`
//! must return 0.
//!
//! The `scope` argument is the partition key, bound by the caller (mcp binds
//! it at startup; contextd resolves it per connection). Wire payloads cannot
//! supply or override the scope — CLAUDE.md DTO discipline.
//!
//! ## Why a write tool and not an extractor
//!
//! A census of a live store found 91.6% of 233k episodes were raw tool
//! telemetry and the community tree had never built above its leaves — no
//! semantic compression had ever run. The fix is not a better summariser;
//! you cannot summarise `ls -la` into wisdom. It is to capture knowledge
//! DIRECTLY, at the moment it is known, from the one participant that
//! actually knows it. There is no per-turn LLM extractor here on purpose:
//! zero added inference cost and nothing on the hot path. The known risk,
//! accepted deliberately, is that it depends on the agent choosing to call it.
//!
//! ## Prose, not an envelope
//!
//! The content is stored as readable prose rather than a JSON payload. The
//! same census found community summaries that were the raw JSON envelope
//! copied verbatim, and the hook has to carry a JSON summariser to undo that
//! at render time. A memory that reads correctly with no renderer in front of
//! it survives every path — recall, injection, a human opening the store.

use lunaris::{EpisodeBuilder, IngestKind};
use serde::{Deserialize, Serialize};

use crate::ServiceError;
use lunaris::Lunaris;
use lunaris_core::Scope;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// The four kinds of knowledge worth keeping (curation-gap decision #3).
///
/// The list is closed on purpose. "Anything the agent thinks is interesting"
/// is how a store fills with 233k tool calls; naming the four kinds is what
/// makes the write path a curation decision rather than another firehose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RememberKind {
    /// A decision and the reasoning behind it.
    Decision,
    /// Something that broke, and what actually fixed it.
    Fix,
    /// How this user wants to work — preferences, style, standing corrections.
    Preference,
    /// Project state, constraints, invariants that bound future work.
    Constraint,
}

impl RememberKind {
    /// The wire name, and the source prefix. One string for both so a memory
    /// cannot be filed under a kind the reader spells differently.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Fix => "fix",
            Self::Preference => "preference",
            Self::Constraint => "constraint",
        }
    }
}

/// Input parameters for `memory.remember`.
///
/// `#[serde(deny_unknown_fields)]` is mandatory (CLAUDE.md §HTTP DTO
/// discipline). The scope field is absent by design — it is bound at server
/// startup and cannot be overridden by the wire payload.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RememberParams {
    /// Which of the four kinds this is.
    pub kind: RememberKind,

    /// What to remember, in the agent's own words.
    pub content: String,

    /// Why it is true or why it was decided. Optional, and the single most
    /// valuable field when present: a fix without its cause is a changelog
    /// line, and a decision without its rationale gets re-litigated.
    #[serde(default)]
    pub why: Option<String>,

    /// Optional tags for categorisation (e.g. `["storage", "perf"]`).
    #[serde(default)]
    pub tags: Option<Vec<String>>,

    /// Optional dedupe key. If present and already seen in this scope, returns
    /// the prior LSN without a second write, so an agent that retries a step
    /// does not double-write the lesson it already recorded.
    #[serde(default)]
    pub dedupe_key: Option<String>,
}

/// Output of a successful `memory.remember` call.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RememberResponse {
    /// Log-sequence number of the committed write (`wall_ms:counter`).
    pub lsn: String,

    /// True if this call returned a previously-committed LSN (dedupe hit).
    #[serde(default)]
    pub was_duplicate: bool,

    /// The source the memory was filed under — `"{kind}:{scope}"`. Returned so
    /// a caller can see how the memory will rank without re-deriving the rule.
    pub source: String,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Render the stored text: the content, then its rationale on its own line.
///
/// Kept separate and pure so the exact bytes a reader will see are testable
/// without a store.
#[must_use]
fn render(content: &str, why: Option<&str>) -> String {
    match why.map(str::trim).filter(|w| !w.is_empty()) {
        Some(why) => format!("{}\n\nWhy: {}", content.trim(), why),
        None => content.trim().to_owned(),
    }
}

/// Execute `memory.remember`.
///
/// Security note: `content`, `why` and `tags` are caller-controlled strings
/// stored as-is. Downstream callers rendering these values MUST treat them as
/// untrusted. `deny_unknown_fields` blocks payload-smuggling.
pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    params: RememberParams,
) -> Result<RememberResponse, ServiceError> {
    if params.content.trim().is_empty() {
        return Err(ServiceError::InvalidInput(
            "remember: content is empty — an empty memory is indistinguishable from a failed \
             capture, and both read as 'nothing was worth keeping'"
                .to_owned(),
        ));
    }

    let source = format!("{}:{}", params.kind.as_str(), scope.as_str());
    let content = render(&params.content, params.why.as_deref());

    let mut meta = serde_json::Map::new();
    meta.insert("kind".into(), serde_json::Value::String(params.kind.as_str().to_owned()));
    if let Some(ref tags) = params.tags {
        meta.insert(
            "tags".into(),
            serde_json::Value::Array(
                tags.iter().map(|t| serde_json::Value::String(t.clone())).collect(),
            ),
        );
    }

    let builder = EpisodeBuilder::new(source.clone(), content).metadata(meta);

    // Re-derive ScopedLunaris per call — never cache it across calls.
    let scoped = lunaris.scoped(scope.clone());

    if let Some(ref key) = params.dedupe_key {
        let (lsn, kind) = scoped.ingest_idempotent(builder, key).await?;
        let was_duplicate = matches!(kind, IngestKind::Duplicate(_));
        tracing::debug!(
            scope     = scope.as_str(),
            lsn       = %lsn,
            dedupe    = %key,
            duplicate = was_duplicate,
            kind      = params.kind.as_str(),
            "memory.remember committed (idempotent path)",
        );
        Ok(RememberResponse { lsn: lsn.to_string(), was_duplicate, source })
    } else {
        let lsn = scoped.ingest(builder).await?;
        tracing::debug!(
            scope = scope.as_str(),
            lsn   = %lsn,
            kind  = params.kind.as_str(),
            "memory.remember committed",
        );
        Ok(RememberResponse { lsn: lsn.to_string(), was_duplicate: false, source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rationale_is_rendered_on_its_own_line() {
        assert_eq!(render("a decision", Some("a reason")), "a decision\n\nWhy: a reason");
    }

    #[test]
    fn an_absent_or_blank_rationale_adds_nothing() {
        assert_eq!(render("a decision", None), "a decision");
        assert_eq!(render("a decision", Some("   ")), "a decision");
        assert!(
            !render("a decision", Some("  ")).contains("Why:"),
            "a blank rationale must not leave a dangling 'Why:' label"
        );
    }

    #[test]
    fn every_kind_has_a_distinct_wire_name() {
        let names: Vec<&str> = [
            RememberKind::Decision,
            RememberKind::Fix,
            RememberKind::Preference,
            RememberKind::Constraint,
        ]
        .iter()
        .map(|k| k.as_str())
        .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "two kinds share a wire name: {names:?}");
    }

    #[test]
    fn the_wire_name_matches_the_serde_representation() {
        // Otherwise the source prefix and the JSON an agent sends disagree,
        // and a memory is filed under a kind nobody can query for.
        for kind in [
            RememberKind::Decision,
            RememberKind::Fix,
            RememberKind::Preference,
            RememberKind::Constraint,
        ] {
            let json = serde_json::to_string(&kind).expect("serialize kind");
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
        }
    }
}

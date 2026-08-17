//! GA-1 — opt-in cross-encoder rerank stage on the production recall root.
//!
//! Mirrors the `LUNARIS_GRAPH_ENABLED` pattern (`graph_pipeline.rs`): the
//! env state is read ONCE at handle construction via a pure decision
//! function (no `std::env::set_var` in tests — edition 2024 makes it
//! `unsafe`, and parallel tests race on process env), stored on the handle,
//! and exposed through a small accessor.
//!
//! Default OFF on every surface. When OFF, the recall path provably never
//! consults the reranker Arc, so the lazy bge-reranker GGUF never loads
//! (pinned by `tests/recall_unified_root.rs::rerank_off_never_touches_the_reranker`).
//! The hook's context-injection hot path never applies the stage regardless
//! of the env (latency-critical; see `lunaris-hook/src/context.rs`).

use crate::handle::Lunaris;

/// Env var gating the rerank stage on MCP `memory.recall` and HTTP `/v1/recall`
/// / SDK `Lunaris::recall()`. Truthy set is EXACTLY the graph toggle's:
/// `"1" | "true" | "TRUE" | "on" | "ON"`; anything else (or unset) is OFF.
pub const RECALL_RERANK_ENV_VAR: &str = "LUNARIS_RECALL_RERANK";

/// Optional depth knob: how many fused candidates feed the cross-encoder.
/// Unset (or `0` / non-numeric) → `2*k`; always clamped to at least the
/// final top-`k` (see `lunaris_retrieve::production_root_reranked`).
pub const RECALL_RERANK_TOP_IN_ENV_VAR: &str = "LUNARIS_RECALL_RERANK_TOP_IN";

/// Frozen-at-construction rerank configuration for the production recall root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecallRerankConfig {
    /// `LUNARIS_RECALL_RERANK` truthy at handle construction.
    pub enabled: bool,
    /// `LUNARIS_RECALL_RERANK_TOP_IN` override; `None` = the `2*k` default.
    pub top_in: Option<usize>,
}

impl RecallRerankConfig {
    /// Pure decision function — `None` = unset env var. Tests pass explicit
    /// values instead of mutating process env (B-1 pattern,
    /// `GraphPipelineHandle::initial_state_from_value`).
    pub fn from_values(enabled_raw: Option<&str>, top_in_raw: Option<&str>) -> Self {
        let enabled = matches!(enabled_raw, Some("1" | "true" | "TRUE" | "on" | "ON"));
        let top_in = top_in_raw.and_then(|s| s.parse::<usize>().ok()).filter(|&n| n > 0);
        Self { enabled, top_in }
    }

    /// Convenience wrapper reading both env vars. ONLY called by the
    /// `Lunaris::open*` constructors at handle-construction time — the
    /// `with_parts*` test seams default OFF (same shape as the graph
    /// pipeline's hardcoded `false` in those seams).
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var(RECALL_RERANK_ENV_VAR).ok().as_deref(),
            std::env::var(RECALL_RERANK_TOP_IN_ENV_VAR).ok().as_deref(),
        )
    }
}

impl Lunaris {
    /// The rerank configuration frozen at handle construction (GA-1).
    pub fn recall_rerank(&self) -> RecallRerankConfig {
        self.recall_rerank
    }

    /// Escape hatch — override the frozen rerank configuration on an
    /// existing handle (programmatic opt-in without env vars; also the test
    /// seam, since edition 2024 forbids safe `std::env::set_var`).
    pub fn with_recall_rerank(mut self, cfg: RecallRerankConfig) -> Self {
        self.recall_rerank = cfg;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_set_matches_graph_toggle() {
        for on in ["1", "true", "TRUE", "on", "ON"] {
            assert!(RecallRerankConfig::from_values(Some(on), None).enabled);
        }
        for off in [Some("0"), Some("off"), Some("yes"), Some(""), None] {
            assert!(!RecallRerankConfig::from_values(off, None).enabled);
        }
    }

    #[test]
    fn top_in_parses_positive_integers_only() {
        assert_eq!(RecallRerankConfig::from_values(None, Some("64")).top_in, Some(64));
        assert_eq!(RecallRerankConfig::from_values(None, Some("0")).top_in, None);
        assert_eq!(RecallRerankConfig::from_values(None, Some("-3")).top_in, None);
        assert_eq!(RecallRerankConfig::from_values(None, Some("x")).top_in, None);
        assert_eq!(RecallRerankConfig::from_values(None, None).top_in, None);
    }
}

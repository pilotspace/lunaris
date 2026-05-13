//! [`ReflectSupervisor`] — per-turn memory calibration via `LlmBackend`.
//!
//! ## Purpose
//!
//! Bi-temporal MVCC stores accumulate facts faster than they invalidate
//! them. After an agent finishes a turn (a coherent ingest + recall +
//! act cycle), a *reflection* pass can:
//!
//! - **Invalidate** facts the turn-end state contradicts (e.g. the user
//!   corrected an earlier statement).
//! - **Boost** chunks that proved load-bearing in the turn so future
//!   recall ranks them higher.
//! - **Pre-warm** a likely-next query so the next turn's first recall
//!   doesn't pay cold-path latency.
//!
//! Reflect is conceptually sibling to `lunaris-verify` (slow-path
//! arbitration on contradiction) and `lunaris-consolidate` (background
//! merge of high-confidence chunks into the graph). It differs by
//! **trigger**: reflect fires per-turn on demand, not per-message
//! (extract) or on a background cadence (consolidate).
//!
//! ## Scope of this commit
//!
//! Scaffold only — DTOs + trait + a minimal `LlmReflectSupervisor`
//! impl that round-trips a JSON-schema-constrained generate call.
//! Wire-up to the turn-end signal lives in a follow-up commit once
//! the upstream `lunaris` umbrella exposes a turn boundary.
//!
//! ## Cost
//!
//! Reflect is the LOWEST-budget LLM call in Lunaris — it runs on a
//! per-turn cadence and shares the same `LlmBackend` instance (likely
//! Gemma-3 4B once the verify-default flip lands). Default
//! [`ReflectOpts::timeout_ms`] is 500 ms; that's loose enough for 4B
//! on CPU and tight enough that a stuck reflection doesn't stall the
//! next turn.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lunaris_core::LunarisError;
use lunaris_llm::{GenOpts, LlmBackend, SchemaConstraint};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Input snapshot the reflection LLM sees. Caller assembles this from
/// the just-finished turn's ingest + recall state. The fields are
/// intentionally minimal — wider context (full chunk text, full
/// recall hits) belongs in the prompt the reflection wrapper builds.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ReflectInput {
    /// ULID identifying the turn. Used for telemetry only — reflect
    /// does NOT key any storage by this; reflections are advisory.
    pub turn_id: Option<Ulid>,
    /// Short summary of what the agent did this turn (the agent or
    /// the ingest pipeline produces this).
    pub turn_summary: String,
    /// Recent fact ulids the turn surfaced. The reflect LLM may
    /// nominate any of these for invalidation if the turn-end state
    /// contradicts them.
    pub recent_fact_ids: Vec<Ulid>,
    /// Recent chunk ulids that proved load-bearing for retrieval.
    /// The reflect LLM may nominate any of these for boost.
    pub recent_chunk_ids: Vec<Ulid>,
}

/// Output the reflection LLM emits. All three fields are advisory —
/// nothing in this commit applies them. Wire-up to the storage layer
/// is a follow-up.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ReflectOutput {
    /// Fact ulids the LLM nominates for invalidation.
    #[serde(default)]
    pub invalidate: Vec<Ulid>,
    /// Chunk ulids the LLM nominates for retrieval-rank boost.
    #[serde(default)]
    pub boost: Vec<Ulid>,
    /// A likely-next query the next turn will issue, so the umbrella
    /// can pre-warm the cache. `None` if the LLM has no signal.
    #[serde(default)]
    pub pre_warm_query: Option<String>,
}

/// Construction options.
#[derive(Clone, Debug)]
pub struct ReflectOpts {
    /// Per-call timeout. 500 ms accommodates 4B-on-CPU; lower for GPU
    /// (~150 ms is reasonable).
    pub timeout_ms: u64,
    /// Max output tokens. Reflect output is short — ulid lists +
    /// optional query — 384 covers ~20 ulids comfortably.
    pub max_tokens: u32,
    /// Sampling temperature. 0.2 = mostly-greedy with a small bit of
    /// variety so identical turns don't always pre-warm the same query
    /// (the calibration loop benefits slightly from diversity).
    pub temperature: f32,
}

impl Default for ReflectOpts {
    fn default() -> Self {
        Self { timeout_ms: 500, max_tokens: 384, temperature: 0.2 }
    }
}

/// Object-safe async reflection supervisor.
#[async_trait]
pub trait ReflectSupervisor: Send + Sync + 'static {
    async fn reflect(&self, input: ReflectInput) -> Result<ReflectOutput, LunarisError>;

    /// `true` if this supervisor produces real reflections; `false`
    /// for a noop so callers can short-circuit prompt assembly.
    fn applies(&self) -> bool {
        true
    }
}

/// Noop reflect supervisor — always returns an empty `ReflectOutput`.
/// Default for `Lunaris::with_reflect` when the umbrella ships
/// reflect-OFF (mirrors blueprint §5.1 default-OFF for verify).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopReflectSupervisor;

#[async_trait]
impl ReflectSupervisor for NoopReflectSupervisor {
    async fn reflect(&self, _input: ReflectInput) -> Result<ReflectOutput, LunarisError> {
        Ok(ReflectOutput::default())
    }
    fn applies(&self) -> bool {
        false
    }
}

/// LLM-backed reflect supervisor. Wraps any `Arc<dyn LlmBackend>` —
/// same trait the extract + verify pipelines consume — so a single
/// candle Gemma-3 4B handle can serve all three when the unified
/// config wires it that way.
#[derive(Clone)]
pub struct LlmReflectSupervisor {
    backend: Arc<dyn LlmBackend>,
    opts: ReflectOpts,
}

impl std::fmt::Debug for LlmReflectSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmReflectSupervisor")
            .field("model_id", &self.backend.model_id())
            .field("opts", &self.opts)
            .finish()
    }
}

impl LlmReflectSupervisor {
    pub fn new(backend: Arc<dyn LlmBackend>) -> Self {
        Self { backend, opts: ReflectOpts::default() }
    }

    pub fn with_opts(backend: Arc<dyn LlmBackend>, opts: ReflectOpts) -> Self {
        Self { backend, opts }
    }
}

#[async_trait]
impl ReflectSupervisor for LlmReflectSupervisor {
    async fn reflect(&self, input: ReflectInput) -> Result<ReflectOutput, LunarisError> {
        let prompt = build_prompt(&input);
        let schema = output_schema();
        let gen_opts = GenOpts {
            max_tokens: self.opts.max_tokens,
            temperature: self.opts.temperature,
            timeout: Duration::from_millis(self.opts.timeout_ms),
        };
        match self.backend.generate(&prompt, SchemaConstraint::JsonSchema(&schema), gen_opts).await
        {
            Ok(decoded) => Ok(parse_reflect_output(&decoded)),
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    model_id = self.backend.model_id(),
                    turn_id = ?input.turn_id,
                    "LlmReflectSupervisor generate failed; emitting empty reflection"
                );
                Ok(ReflectOutput::default())
            }
        }
    }

    fn applies(&self) -> bool {
        self.backend.applies()
    }
}

fn build_prompt(input: &ReflectInput) -> String {
    // Compact prompt — full chunk/fact bodies are NOT inlined here. The
    // reflection LLM works from ulids + the agent-supplied summary; if
    // it wants more it asks the next turn for it. This keeps the
    // per-turn token budget low.
    let facts = input.recent_fact_ids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(", ");
    let chunks =
        input.recent_chunk_ids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(", ");
    format!(
        "You are calibrating an agent memory store after a turn finished.\n\
         Turn summary:\n{summary}\n\n\
         Recent fact ulids: [{facts}]\n\
         Recent chunk ulids: [{chunks}]\n\n\
         Respond with a JSON object:\n\
         {{\"invalidate\": [<fact ulids to invalidate>],\
          \"boost\": [<chunk ulids to boost>],\
          \"pre_warm_query\": <string or null>}}\n\
         If nothing should change, emit empty arrays and a null query.",
        summary = input.turn_summary
    )
}

fn output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "invalidate": {"type": "array", "items": {"type": "string"}},
            "boost":      {"type": "array", "items": {"type": "string"}},
            "pre_warm_query": {"type": ["string", "null"]}
        },
        "required": ["invalidate", "boost", "pre_warm_query"]
    })
}

fn parse_reflect_output(decoded: &str) -> ReflectOutput {
    let Some(start) = decoded.find('{') else {
        return ReflectOutput::default();
    };
    let bytes = decoded.as_bytes();
    let mut depth = 0_i32;
    let mut end_excl = start;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end_excl = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end_excl == start {
        return ReflectOutput::default();
    }
    let json_slice = &decoded[start..end_excl];

    #[derive(Deserialize)]
    struct Wire {
        #[serde(default)]
        invalidate: Vec<String>,
        #[serde(default)]
        boost: Vec<String>,
        #[serde(default)]
        pre_warm_query: Option<String>,
    }
    match serde_json::from_str::<Wire>(json_slice) {
        Ok(w) => ReflectOutput {
            invalidate: w
                .invalidate
                .into_iter()
                .filter_map(|s| Ulid::from_string(&s).ok())
                .collect(),
            boost: w.boost.into_iter().filter_map(|s| Ulid::from_string(&s).ok()).collect(),
            pre_warm_query: w.pre_warm_query.filter(|s| !s.is_empty()),
        },
        Err(e) => {
            tracing::warn!(err = %e, "LlmReflectSupervisor JSON parse failed; emitting empty");
            ReflectOutput::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubBackend {
        out: String,
    }

    #[async_trait]
    impl LlmBackend for StubBackend {
        async fn generate(
            &self,
            _prompt: &str,
            _constraint: SchemaConstraint<'_>,
            _opts: GenOpts,
        ) -> Result<String, LunarisError> {
            Ok(self.out.clone())
        }
        fn model_id(&self) -> &str {
            "stub://reflect"
        }
    }

    #[tokio::test]
    async fn noop_supervisor_returns_empty() {
        let s = NoopReflectSupervisor;
        let out = s.reflect(ReflectInput::default()).await.unwrap();
        assert_eq!(out, ReflectOutput::default());
        assert!(!s.applies());
    }

    #[tokio::test]
    async fn parses_valid_reflect_json() {
        let fact = Ulid::new();
        let chunk = Ulid::new();
        let out_json = format!(
            r#"{{"invalidate":["{fact}"],"boost":["{chunk}"],"pre_warm_query":"who is Alice?"}}"#
        );
        let backend: Arc<dyn LlmBackend> = Arc::new(StubBackend { out: out_json });
        let supervisor = LlmReflectSupervisor::new(backend);
        let out = supervisor
            .reflect(ReflectInput {
                turn_summary: "agent answered question".into(),
                ..ReflectInput::default()
            })
            .await
            .unwrap();
        assert_eq!(out.invalidate, vec![fact]);
        assert_eq!(out.boost, vec![chunk]);
        assert_eq!(out.pre_warm_query.as_deref(), Some("who is Alice?"));
    }

    #[tokio::test]
    async fn malformed_output_emits_empty() {
        let backend: Arc<dyn LlmBackend> =
            Arc::new(StubBackend { out: "definitely not json".into() });
        let supervisor = LlmReflectSupervisor::new(backend);
        let out = supervisor.reflect(ReflectInput::default()).await.unwrap();
        assert_eq!(out, ReflectOutput::default());
    }

    #[tokio::test]
    async fn invalid_ulids_in_output_are_dropped() {
        let valid = Ulid::new();
        let out_json = format!(
            r#"{{"invalidate":["{valid}","not-a-ulid"],"boost":[],"pre_warm_query":null}}"#
        );
        let backend: Arc<dyn LlmBackend> = Arc::new(StubBackend { out: out_json });
        let supervisor = LlmReflectSupervisor::new(backend);
        let out = supervisor.reflect(ReflectInput::default()).await.unwrap();
        assert_eq!(out.invalidate, vec![valid]);
        assert!(out.boost.is_empty());
        assert!(out.pre_warm_query.is_none());
    }

    #[test]
    fn output_schema_has_required_fields() {
        let schema = output_schema();
        let required = schema["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"invalidate"));
        assert!(names.contains(&"boost"));
        assert!(names.contains(&"pre_warm_query"));
    }
}

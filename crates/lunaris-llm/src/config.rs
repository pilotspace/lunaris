//! [`LlmConfig`] — unified provider + model selection for the three
//! LLM-using pipelines (`extract`, `verify`, `reflect`).
//!
//! ## Env precedence
//!
//! For each pipeline `P`, the resolved (provider, model) pair is:
//!
//! 1. `LUNARIS_{P}_PROVIDER` / `LUNARIS_{P}_MODEL` (per-pipeline override)
//! 2. `LUNARIS_LLM_PROVIDER`  / `LUNARIS_LLM_MODEL`  (workspace default)
//! 3. The hard-coded default for that pipeline ([`Pipeline::default_model`]).
//!
//! ## Config file
//!
//! If `LUNARIS_LLM_CONFIG=/path/to/llm.toml` is set, the file is loaded
//! and merged with env. Env vars WIN over file values — env is the
//! deployment-time override surface (Docker, K8s, systemd).
//!
//! ```toml
//! # llm.toml — every section is optional. Anything omitted falls through
//! # to env, then to the hard-coded pipeline default.
//! [default]
//! provider = "candle"
//! model    = "gemma-3-4b-it"
//!
//! [extract]
//! # inherits provider from [default], overrides model
//! model = "gemma-3-4b-it"
//!
//! [verify]
//! # opt back in to the slow-path 27B model
//! provider = "candle"
//! model    = "gemma-3-27b-it"
//!
//! [reflect]
//! provider = "ollama"
//! model    = "gemma3:4b"
//! ```
//!
//! ## What this file does NOT do
//!
//! - **Construct backends.** That's the consumer's job — this module
//!   answers "which provider + which model" only. Backend construction
//!   (loading weights, opening an HTTP client) belongs to the
//!   pipeline-side `with_backend(...)` builders, because they own the
//!   lifecycle.
//! - **Read API keys.** Provider-specific secrets stay under each
//!   provider's existing env name (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
//!   `GEMINI_API_KEY`). Concentrating secrets here would create a
//!   migration trap for existing deployments.

use std::env;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Which LLM pipeline a config applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pipeline {
    Extract,
    Verify,
    Reflect,
}

impl Pipeline {
    /// Env-var prefix segment. `Pipeline::Extract` → `"EXTRACT"`.
    fn env_segment(self) -> &'static str {
        match self {
            Pipeline::Extract => "EXTRACT",
            Pipeline::Verify => "VERIFY",
            Pipeline::Reflect => "REFLECT",
        }
    }

    /// Hard-coded default `(provider, model)` for this pipeline, used
    /// when neither env nor config file specifies one.
    ///
    /// **Note on verify**: the default returned here is still 27B; the
    /// behavior flip to 4B is gated on the ER-F1 quality test and lands
    /// in a later commit. See `crates/lunaris-llm/README.md` (TBD) for
    /// the flip rationale.
    pub fn default_provider_and_model(self) -> (ProviderKind, &'static str) {
        match self {
            Pipeline::Extract => (ProviderKind::Candle, "gemma-3-4b-it"),
            Pipeline::Verify => (ProviderKind::Candle, "gemma-3-27b-it"),
            // Reflect ships 4B from day one — it's a new code path, no
            // legacy default to preserve.
            Pipeline::Reflect => (ProviderKind::Candle, "gemma-3-4b-it"),
        }
    }
}

/// LLM provider transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// In-process candle inference.
    Candle,
    /// Ollama HTTP server.
    Ollama,
    /// Anthropic cloud API.
    Anthropic,
    /// OpenAI cloud API.
    OpenAI,
    /// Google Gemini cloud API.
    Gemini,
}

impl FromStr for ProviderKind {
    type Err = LlmConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "candle" => Ok(ProviderKind::Candle),
            "ollama" => Ok(ProviderKind::Ollama),
            "anthropic" => Ok(ProviderKind::Anthropic),
            "openai" => Ok(ProviderKind::OpenAI),
            "gemini" | "google" => Ok(ProviderKind::Gemini),
            other => Err(LlmConfigError::UnknownProvider(other.to_string())),
        }
    }
}

/// Resolved configuration for a single pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmConfig {
    pub pipeline: Pipeline,
    pub provider: ProviderKind,
    pub model: String,
}

impl LlmConfig {
    /// Resolve `(provider, model)` for `pipeline` by walking the
    /// precedence chain: per-pipeline env → workspace env → optional
    /// TOML file → hard-coded default.
    ///
    /// Reading env via `std::env::var` is intentional — we don't want a
    /// background thread mutating `LUNARIS_LLM_*` after construction;
    /// see CLAUDE.md `#![forbid(unsafe_code)]` (the unsafe `set_var`
    /// pattern is rejected in lunaris-core::hf-hub setup for the same
    /// reason).
    pub fn resolve(pipeline: Pipeline) -> Result<Self, LlmConfigError> {
        let file = load_config_file()?;
        Self::resolve_with(pipeline, &file, EnvSource::Process)
    }

    /// Same as [`resolve`](Self::resolve) but reads env from a caller-
    /// supplied map. Lets tests pin behaviour without touching process
    /// env (which is shared across the test binary).
    pub fn resolve_with(
        pipeline: Pipeline,
        file: &LlmConfigFile,
        env: EnvSource<'_>,
    ) -> Result<Self, LlmConfigError> {
        let (default_provider, default_model) = pipeline.default_provider_and_model();
        let segment = pipeline.env_segment();

        // Per-pipeline env override wins.
        let provider_env =
            env.get(&format!("LUNARIS_{segment}_PROVIDER"))
                .or_else(|| env.get("LUNARIS_LLM_PROVIDER"));
        let model_env = env
            .get(&format!("LUNARIS_{segment}_MODEL"))
            .or_else(|| env.get("LUNARIS_LLM_MODEL"));

        // Config-file values, scoped first to the pipeline section, then
        // to [default].
        let section = file.section(pipeline);
        let provider_file = section
            .and_then(|s| s.provider)
            .or(file.default.as_ref().and_then(|d| d.provider));
        let model_file = section
            .and_then(|s| s.model.clone())
            .or_else(|| file.default.as_ref().and_then(|d| d.model.clone()));

        let provider = match provider_env {
            Some(s) => s.parse::<ProviderKind>()?,
            None => provider_file.unwrap_or(default_provider),
        };
        let model = model_env
            .map(|s| s.to_string())
            .or(model_file)
            .unwrap_or_else(|| default_model.to_string());

        Ok(Self { pipeline, provider, model })
    }
}

/// Where [`LlmConfig::resolve_with`] reads environment from.
#[derive(Debug, Clone, Copy)]
pub enum EnvSource<'a> {
    /// Real process env via `std::env::var`.
    Process,
    /// Test-supplied map.
    Map(&'a [(&'a str, &'a str)]),
}

impl<'a> EnvSource<'a> {
    fn get(&self, key: &str) -> Option<String> {
        match self {
            EnvSource::Process => env::var(key).ok(),
            EnvSource::Map(entries) => entries
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string()),
        }
    }
}

/// Parsed contents of `$LUNARIS_LLM_CONFIG`.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LlmConfigFile {
    #[serde(default)]
    pub default: Option<PipelineSection>,
    #[serde(default)]
    pub extract: Option<PipelineSection>,
    #[serde(default)]
    pub verify: Option<PipelineSection>,
    #[serde(default)]
    pub reflect: Option<PipelineSection>,
}

impl LlmConfigFile {
    fn section(&self, p: Pipeline) -> Option<&PipelineSection> {
        match p {
            Pipeline::Extract => self.extract.as_ref(),
            Pipeline::Verify => self.verify.as_ref(),
            Pipeline::Reflect => self.reflect.as_ref(),
        }
    }

    /// Parse a TOML string. Public for tests and for callers that
    /// already have the file contents in memory.
    pub fn parse(toml_src: &str) -> Result<Self, LlmConfigError> {
        toml::from_str(toml_src).map_err(|e| LlmConfigError::ParseFile(e.to_string()))
    }
}

/// One pipeline section of the config file.
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineSection {
    #[serde(default)]
    pub provider: Option<ProviderKind>,
    #[serde(default)]
    pub model: Option<String>,
}

fn load_config_file() -> Result<LlmConfigFile, LlmConfigError> {
    let Some(path) = env::var("LUNARIS_LLM_CONFIG").ok() else {
        return Ok(LlmConfigFile::default());
    };
    if path.is_empty() {
        return Ok(LlmConfigFile::default());
    }
    let src = std::fs::read_to_string(Path::new(&path))
        .map_err(|e| LlmConfigError::ReadFile { path: path.clone(), cause: e.to_string() })?;
    LlmConfigFile::parse(&src)
}

/// Errors surfaced by [`LlmConfig::resolve`].
///
/// I/O details from `LUNARIS_LLM_CONFIG` reads are stringified into `cause`
/// rather than nested as `#[source]` — `std::io::Error` is non-Clone and
/// would force this whole enum non-Clone. Field is named `cause` (not
/// `source`) so thiserror 2.x doesn't auto-promote it to `#[source]`.
#[derive(Debug, Error)]
pub enum LlmConfigError {
    #[error("unknown LLM provider: `{0}` (expected one of: candle, ollama, anthropic, openai, gemini)")]
    UnknownProvider(String),
    #[error("failed to read LLM config file `{path}`: {cause}")]
    ReadFile { path: String, cause: String },
    #[error("failed to parse LLM config TOML: {0}")]
    ParseFile(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_today_hardcoded_pipeline_defaults() {
        // Pin the current defaults so a silent flip (especially Verify →
        // 4B) requires editing the test, not just the const.
        assert_eq!(
            Pipeline::Extract.default_provider_and_model(),
            (ProviderKind::Candle, "gemma-3-4b-it")
        );
        assert_eq!(
            Pipeline::Verify.default_provider_and_model(),
            (ProviderKind::Candle, "gemma-3-27b-it")
        );
        assert_eq!(
            Pipeline::Reflect.default_provider_and_model(),
            (ProviderKind::Candle, "gemma-3-4b-it")
        );
    }

    #[test]
    fn provider_kind_parses_case_insensitive() {
        assert_eq!("candle".parse::<ProviderKind>().unwrap(), ProviderKind::Candle);
        assert_eq!("CANDLE".parse::<ProviderKind>().unwrap(), ProviderKind::Candle);
        assert_eq!("ollama".parse::<ProviderKind>().unwrap(), ProviderKind::Ollama);
        assert_eq!("anthropic".parse::<ProviderKind>().unwrap(), ProviderKind::Anthropic);
        assert_eq!("openai".parse::<ProviderKind>().unwrap(), ProviderKind::OpenAI);
        assert_eq!("gemini".parse::<ProviderKind>().unwrap(), ProviderKind::Gemini);
        assert_eq!("google".parse::<ProviderKind>().unwrap(), ProviderKind::Gemini);
    }

    #[test]
    fn provider_kind_rejects_unknown() {
        let err = "qwen".parse::<ProviderKind>().unwrap_err();
        assert!(matches!(err, LlmConfigError::UnknownProvider(_)));
    }

    #[test]
    fn env_overrides_default() {
        let file = LlmConfigFile::default();
        let env = EnvSource::Map(&[
            ("LUNARIS_VERIFY_PROVIDER", "ollama"),
            ("LUNARIS_VERIFY_MODEL", "gemma3:4b"),
        ]);
        let cfg = LlmConfig::resolve_with(Pipeline::Verify, &file, env).unwrap();
        assert_eq!(cfg.provider, ProviderKind::Ollama);
        assert_eq!(cfg.model, "gemma3:4b");
    }

    #[test]
    fn per_pipeline_env_overrides_workspace_env() {
        let file = LlmConfigFile::default();
        let env = EnvSource::Map(&[
            ("LUNARIS_LLM_PROVIDER", "anthropic"),
            ("LUNARIS_LLM_MODEL", "claude-3-5-sonnet-latest"),
            ("LUNARIS_EXTRACT_PROVIDER", "candle"),
            ("LUNARIS_EXTRACT_MODEL", "gemma-3-4b-it"),
        ]);
        let extract = LlmConfig::resolve_with(Pipeline::Extract, &file, env).unwrap();
        assert_eq!(extract.provider, ProviderKind::Candle);
        assert_eq!(extract.model, "gemma-3-4b-it");
        // Verify has no per-pipeline override → falls through to workspace.
        let verify = LlmConfig::resolve_with(Pipeline::Verify, &file, env).unwrap();
        assert_eq!(verify.provider, ProviderKind::Anthropic);
        assert_eq!(verify.model, "claude-3-5-sonnet-latest");
    }

    #[test]
    fn config_file_used_when_env_absent() {
        let file = LlmConfigFile::parse(
            r#"
            [default]
            provider = "ollama"
            model = "gemma3:4b"

            [verify]
            provider = "candle"
            model = "gemma-3-27b-it"
            "#,
        )
        .unwrap();
        let env = EnvSource::Map(&[]);
        let extract = LlmConfig::resolve_with(Pipeline::Extract, &file, env).unwrap();
        // Extract has no [extract] section → inherits [default].
        assert_eq!(extract.provider, ProviderKind::Ollama);
        assert_eq!(extract.model, "gemma3:4b");
        let verify = LlmConfig::resolve_with(Pipeline::Verify, &file, env).unwrap();
        assert_eq!(verify.provider, ProviderKind::Candle);
        assert_eq!(verify.model, "gemma-3-27b-it");
    }

    #[test]
    fn env_wins_over_config_file() {
        let file = LlmConfigFile::parse(
            r#"
            [extract]
            provider = "candle"
            model = "gemma-3-4b-it"
            "#,
        )
        .unwrap();
        let env = EnvSource::Map(&[("LUNARIS_EXTRACT_PROVIDER", "ollama")]);
        let cfg = LlmConfig::resolve_with(Pipeline::Extract, &file, env).unwrap();
        // Env provider wins; model falls back to file (file ≻ default).
        assert_eq!(cfg.provider, ProviderKind::Ollama);
        assert_eq!(cfg.model, "gemma-3-4b-it");
    }

    #[test]
    fn unknown_env_provider_is_a_typed_error() {
        let file = LlmConfigFile::default();
        let env = EnvSource::Map(&[("LUNARIS_EXTRACT_PROVIDER", "qwen")]);
        let err = LlmConfig::resolve_with(Pipeline::Extract, &file, env).unwrap_err();
        match err {
            LlmConfigError::UnknownProvider(s) => assert_eq!(s, "qwen"),
            other => panic!("expected UnknownProvider, got {other:?}"),
        }
    }

    #[test]
    fn unknown_toml_field_is_rejected() {
        // `#[serde(deny_unknown_fields)]` mirrors the lunaris-server DTO
        // discipline in CLAUDE.md.
        let err = LlmConfigFile::parse(
            r#"
            [extract]
            povider = "candle"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, LlmConfigError::ParseFile(_)));
    }
}

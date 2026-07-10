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
//! provider = "ollama"
//! model    = "gemma3:4b"
//!
//! [extract]
//! # inherits provider from [default], overrides model
//! model = "gemma3:4b"
//!
//! [verify]
//! provider = "anthropic"
//! model    = "claude-3-5-sonnet-latest"
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
    /// llama.cpp-only cutover (2026-07, Phase C): the in-process candle
    /// backends are gone and the LLM slots are remote-only, so the
    /// zero-config default is the keyless local-remote transport — an
    /// Ollama server on localhost with Gemma-3 4B. Operators pick a cloud
    /// provider via `LUNARIS_LLM_PROVIDER` / per-pipeline env or the
    /// config file. Note the production `Lunaris::open()` path treats
    /// "no provider env set" as degraded (Noop) — this default only
    /// shapes `LlmConfig` resolution for callers that use it directly.
    pub fn default_provider_and_model(self) -> (ProviderKind, &'static str) {
        match self {
            Pipeline::Extract => (ProviderKind::Ollama, "gemma3:4b"),
            Pipeline::Verify => (ProviderKind::Ollama, "gemma3:4b"),
            Pipeline::Reflect => (ProviderKind::Ollama, "gemma3:4b"),
        }
    }
}

/// LLM provider transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// Ollama HTTP server.
    Ollama,
    /// Anthropic cloud API.
    Anthropic,
    /// OpenAI cloud API.
    OpenAI,
    /// Google Gemini cloud API.
    Gemini,
    /// Any OpenAI-compatible `/chat/completions` server at a caller-supplied
    /// base URL (Ollama `/v1`, llama-server, vLLM, LM Studio). The base URL
    /// comes from `LUNARIS_OPENAI_COMPAT_BASE_URL`; the API key
    /// (`LUNARIS_OPENAI_COMPAT_API_KEY`) is OPTIONAL — local servers are
    /// typically unauthenticated.
    #[serde(rename = "openai-compat")]
    OpenAiCompat,
}

impl FromStr for ProviderKind {
    type Err = LlmConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ollama" => Ok(ProviderKind::Ollama),
            "anthropic" => Ok(ProviderKind::Anthropic),
            "openai" => Ok(ProviderKind::OpenAI),
            "gemini" | "google" => Ok(ProviderKind::Gemini),
            "openai-compat" | "openai-compatible" => Ok(ProviderKind::OpenAiCompat),
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
        let provider_env = env
            .get(&format!("LUNARIS_{segment}_PROVIDER"))
            .or_else(|| env.get("LUNARIS_LLM_PROVIDER"));
        let model_env =
            env.get(&format!("LUNARIS_{segment}_MODEL")).or_else(|| env.get("LUNARIS_LLM_MODEL"));

        // Config-file values, scoped first to the pipeline section, then
        // to [default].
        let section = file.section(pipeline);
        let provider_file =
            section.and_then(|s| s.provider).or(file.default.as_ref().and_then(|d| d.provider));
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
            EnvSource::Map(entries) => {
                entries.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_string())
            }
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
    #[error(
        "unknown LLM provider: `{0}` (expected one of: ollama, anthropic, openai, \
         gemini, openai-compat)"
    )]
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
        // Pin the current defaults so a silent flip requires editing the
        // test, not just the const. llama.cpp-only cutover: the LLM slots
        // are remote-only, so the zero-config default is the keyless
        // localhost transport (Ollama + Gemma-3 4B) for all three
        // pipelines.
        assert_eq!(
            Pipeline::Extract.default_provider_and_model(),
            (ProviderKind::Ollama, "gemma3:4b")
        );
        assert_eq!(
            Pipeline::Verify.default_provider_and_model(),
            (ProviderKind::Ollama, "gemma3:4b")
        );
        assert_eq!(
            Pipeline::Reflect.default_provider_and_model(),
            (ProviderKind::Ollama, "gemma3:4b")
        );
    }

    #[test]
    fn provider_kind_parses_case_insensitive() {
        assert_eq!("ollama".parse::<ProviderKind>().unwrap(), ProviderKind::Ollama);
        assert_eq!("OLLAMA".parse::<ProviderKind>().unwrap(), ProviderKind::Ollama);
        assert_eq!("anthropic".parse::<ProviderKind>().unwrap(), ProviderKind::Anthropic);
        assert_eq!("openai".parse::<ProviderKind>().unwrap(), ProviderKind::OpenAI);
        assert_eq!("gemini".parse::<ProviderKind>().unwrap(), ProviderKind::Gemini);
        assert_eq!("google".parse::<ProviderKind>().unwrap(), ProviderKind::Gemini);
    }

    #[test]
    fn openai_compat_parses_from_env_string_and_toml() {
        // A3 (llama.cpp-only cutover): extractor/verifier go remote-only —
        // cloud mux + ONE generic OpenAI-compatible URL backend. Operators
        // select it exactly like any other provider.
        assert_eq!("openai-compat".parse::<ProviderKind>().unwrap(), ProviderKind::OpenAiCompat);
        assert_eq!("OPENAI-COMPAT".parse::<ProviderKind>().unwrap(), ProviderKind::OpenAiCompat);

        let file = LlmConfigFile::parse(
            r#"
            [extract]
            provider = "openai-compat"
            model = "qwen3:4b"
            "#,
        )
        .unwrap();
        let cfg = LlmConfig::resolve_with(Pipeline::Extract, &file, EnvSource::Map(&[])).unwrap();
        assert_eq!(cfg.provider, ProviderKind::OpenAiCompat);
        assert_eq!(cfg.model, "qwen3:4b");
    }

    #[test]
    fn provider_kind_rejects_unknown() {
        let err = "qwen".parse::<ProviderKind>().unwrap_err();
        assert!(matches!(err, LlmConfigError::UnknownProvider(_)));
        // llama.cpp-only cutover: the in-process candle provider is GONE —
        // a leftover `provider = "candle"` config must fail loudly, not
        // silently map to some other transport.
        let err = "candle".parse::<ProviderKind>().unwrap_err();
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
            ("LUNARIS_EXTRACT_PROVIDER", "openai-compat"),
            ("LUNARIS_EXTRACT_MODEL", "qwen3:4b"),
        ]);
        let extract = LlmConfig::resolve_with(Pipeline::Extract, &file, env).unwrap();
        assert_eq!(extract.provider, ProviderKind::OpenAiCompat);
        assert_eq!(extract.model, "qwen3:4b");
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
            provider = "anthropic"
            model = "claude-3-5-sonnet-latest"
            "#,
        )
        .unwrap();
        let env = EnvSource::Map(&[]);
        let extract = LlmConfig::resolve_with(Pipeline::Extract, &file, env).unwrap();
        // Extract has no [extract] section → inherits [default].
        assert_eq!(extract.provider, ProviderKind::Ollama);
        assert_eq!(extract.model, "gemma3:4b");
        let verify = LlmConfig::resolve_with(Pipeline::Verify, &file, env).unwrap();
        assert_eq!(verify.provider, ProviderKind::Anthropic);
        assert_eq!(verify.model, "claude-3-5-sonnet-latest");
    }

    #[test]
    fn env_wins_over_config_file() {
        let file = LlmConfigFile::parse(
            r#"
            [extract]
            provider = "anthropic"
            model = "claude-3-5-haiku-latest"
            "#,
        )
        .unwrap();
        let env = EnvSource::Map(&[("LUNARIS_EXTRACT_PROVIDER", "ollama")]);
        let cfg = LlmConfig::resolve_with(Pipeline::Extract, &file, env).unwrap();
        // Env provider wins; model falls back to file (file ≻ default).
        assert_eq!(cfg.provider, ProviderKind::Ollama);
        assert_eq!(cfg.model, "claude-3-5-haiku-latest");
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
            povider = "ollama"
            "#,
        )
        .unwrap_err();
        assert!(matches!(err, LlmConfigError::ParseFile(_)));
    }
}

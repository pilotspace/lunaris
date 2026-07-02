//! OpenAI-compatible `/embeddings` remote embedder.
//!
//! Talks to any server implementing the OpenAI embeddings wire contract
//! (`POST {base_url}/v1/embeddings` with `{"model", "input": [..]}` →
//! `{"data": [{"embedding": [..], "index": n}, ..]}`). This covers OpenAI
//! itself, Azure-OpenAI-compatible gateways, vLLM, LiteLLM, text-embeddings-
//! inference, llama.cpp `--embeddings`, and most local OpenAI shims.
//!
//! Selected by `resolve_embedder()` when `LUNARIS_EMBEDDER_OPENAI_URL` is set
//! (takes precedence over the Ollama hatch). This is the supported remote path
//! after the candle embedder was made optional (`native` feature off).
//!
//! ## Design for failure (CLAUDE.md)
//!
//! - **Timeout**: 10s per request (reqwest client-level).
//! - **Retries**: up to [`MAX_ATTEMPTS`] with exponential backoff on transient
//!   failures (transport errors, HTTP 429, HTTP 5xx). 4xx / parse / shape
//!   errors are fatal and never retried (retrying a bad request just burns the
//!   budget and hammers the server).
//! - **Circuit breaker**: [`Breaker`] opens after [`BREAKER_THRESHOLD`]
//!   consecutive request failures and fast-fails for [`BREAKER_COOLDOWN`],
//!   then half-opens for a single trial. Keeps a wedged endpoint from stalling
//!   every recall with the full timeout×retry ladder.
//! - **Rollback**: embeddings are validated for row-count and per-row dimension
//!   before returning; a spoofed / misconfigured server surfaces as an error
//!   instead of silently corrupting the vector index (no partial write).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use lunaris_core::{Embedder, LunarisError, StorageError};
use serde::{Deserialize, Serialize};

/// Endpoint for the OpenAI-compatible embedder. When set, `resolve_embedder()`
/// routes through [`OpenAiEmbedder`] ahead of the Ollama hatch and the native
/// default.
pub const OPENAI_URL_ENV_VAR: &str = "LUNARIS_EMBEDDER_OPENAI_URL";
/// Model id sent in the request body.
pub const OPENAI_MODEL_ENV_VAR: &str = "LUNARIS_EMBEDDER_OPENAI_MODEL";
/// Bearer token. Optional — many local OpenAI shims accept unauthenticated
/// requests; only sent when non-empty.
pub const OPENAI_API_KEY_ENV_VAR: &str = "LUNARIS_EMBEDDER_OPENAI_API_KEY";

/// Default model when [`OPENAI_MODEL_ENV_VAR`] is unset.
pub const DEFAULT_OPENAI_MODEL: &str = "text-embedding-3-small";
/// Default expected dim — matches granite-r2 (768-d) so a swapped embedder
/// stays index-interoperable. Override via [`OpenAiEmbedderOpts::dim`].
pub const DEFAULT_DIM: usize = 768;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// 1 initial attempt + 2 retries.
const MAX_ATTEMPTS: usize = 3;
const BACKOFF_BASE: Duration = Duration::from_millis(100);
const BREAKER_THRESHOLD: usize = 5;
const BREAKER_COOLDOWN: Duration = Duration::from_secs(30);

fn backend_err(msg: impl Into<String>) -> LunarisError {
    LunarisError::Storage(StorageError::Backend(msg.into()))
}

/// Construction options for [`OpenAiEmbedder`]. `Default` reads the env vars
/// documented on the `*_ENV_VAR` constants.
#[derive(Clone)]
pub struct OpenAiEmbedderOpts {
    pub base_url: String,
    pub model: String,
    /// Bearer token; `None` or empty → request sent without an `Authorization`
    /// header.
    pub api_key: Option<String>,
    pub dim: usize,
}

impl Default for OpenAiEmbedderOpts {
    fn default() -> Self {
        let base_url = std::env::var(OPENAI_URL_ENV_VAR).ok().unwrap_or_default();
        let model = std::env::var(OPENAI_MODEL_ENV_VAR)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string());
        let api_key = std::env::var(OPENAI_API_KEY_ENV_VAR).ok().filter(|s| !s.trim().is_empty());
        Self { base_url, model, api_key, dim: DEFAULT_DIM }
    }
}

// Redact the bearer token so it never lands in logs / panic messages.
impl std::fmt::Debug for OpenAiEmbedderOpts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiEmbedderOpts")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("dim", &self.dim)
            .finish()
    }
}

/// Simple consecutive-failure circuit breaker. Unit-testable in isolation
/// (no HTTP) — see `tests::breaker_*`.
#[derive(Debug)]
struct Breaker {
    threshold: usize,
    cooldown: Duration,
    inner: Mutex<BreakerInner>,
}

#[derive(Debug, Default)]
struct BreakerInner {
    consecutive_failures: usize,
    open_until: Option<Instant>,
}

impl Breaker {
    fn new(threshold: usize, cooldown: Duration) -> Self {
        Self { threshold, cooldown, inner: Mutex::new(BreakerInner::default()) }
    }

    /// Fast-fail while the breaker is open. Transitions open→half-open once the
    /// cooldown elapses (clears `open_until` so exactly one trial is allowed).
    fn check(&self) -> Result<(), LunarisError> {
        let mut g = self.inner.lock().expect("breaker mutex poisoned");
        if let Some(until) = g.open_until {
            if Instant::now() < until {
                return Err(backend_err(
                    "lunaris-embed-remote openai: circuit breaker open (endpoint failing)",
                ));
            }
            // Half-open: allow a single trial through.
            g.open_until = None;
        }
        Ok(())
    }

    fn record_success(&self) {
        let mut g = self.inner.lock().expect("breaker mutex poisoned");
        g.consecutive_failures = 0;
        g.open_until = None;
    }

    fn record_failure(&self) {
        let mut g = self.inner.lock().expect("breaker mutex poisoned");
        g.consecutive_failures = g.consecutive_failures.saturating_add(1);
        if g.consecutive_failures >= self.threshold {
            g.open_until = Some(Instant::now() + self.cooldown);
        }
    }
}

/// OpenAI-compatible HTTP `Embedder`. Cheap to clone the config but holds the
/// breaker + client by `Arc` so all clones share failure state.
#[derive(Clone)]
pub struct OpenAiEmbedder {
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    dim: usize,
    breaker: std::sync::Arc<Breaker>,
}

impl std::fmt::Debug for OpenAiEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiEmbedder")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("dim", &self.dim)
            .finish()
    }
}

impl OpenAiEmbedder {
    /// Construct an OpenAI-compatible embedder. Fails fast if `base_url` is
    /// empty (misconfiguration) or the HTTP client cannot be built.
    pub fn new(opts: OpenAiEmbedderOpts) -> Result<Self, LunarisError> {
        if opts.base_url.trim().is_empty() {
            return Err(backend_err(
                "lunaris-embed-remote openai: base_url is empty (set LUNARIS_EMBEDDER_OPENAI_URL)",
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| backend_err(format!("lunaris-embed-remote openai client: {e}")))?;
        tracing::info!(
            target: "lunaris::embed_remote",
            base_url = %opts.base_url,
            model = %opts.model,
            dim = opts.dim,
            authenticated = opts.api_key.is_some(),
            "OpenAiEmbedder constructed (remote OpenAI-compatible /embeddings)"
        );
        Ok(Self {
            client,
            base_url: opts.base_url,
            model: opts.model,
            api_key: opts.api_key,
            dim: opts.dim,
            breaker: std::sync::Arc::new(Breaker::new(BREAKER_THRESHOLD, BREAKER_COOLDOWN)),
        })
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/embeddings", self.base_url.trim_end_matches('/'))
    }

    /// One HTTP round-trip. `Err(true, _)` is transient (retry-worthy),
    /// `Err(false, _)` is fatal (config / shape — do not retry).
    async fn try_once(
        &self,
        url: &str,
        inputs: &[&str],
    ) -> Result<Vec<Vec<f32>>, (bool, LunarisError)> {
        let body = EmbedRequest { model: &self.model, input: inputs };
        let mut req = self.client.post(url).json(&body);
        if let Some(key) = self.api_key.as_deref() {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            // Transport errors (DNS, connect, timeout) are transient.
            .map_err(|e| (true, backend_err(format!("lunaris-embed-remote openai: {e}"))))?;

        let status = resp.status();
        if !status.is_success() {
            // 429 + 5xx are transient; other 4xx are fatal.
            let transient = status.as_u16() == 429 || status.is_server_error();
            return Err((
                transient,
                backend_err(format!("lunaris-embed-remote openai: HTTP {status}")),
            ));
        }

        // Parse / shape failures are fatal — a well-formed 200 with the wrong
        // body won't fix itself on retry.
        let parsed: EmbedResponse = resp
            .json()
            .await
            .map_err(|e| (false, backend_err(format!("lunaris-embed-remote openai parse: {e}"))))?;
        let rows = parsed.into_ordered_rows();
        self.validate(&rows, inputs.len()).map_err(|e| (false, e))?;
        Ok(rows)
    }

    fn validate(&self, rows: &[Vec<f32>], expected_rows: usize) -> Result<(), LunarisError> {
        if rows.len() != expected_rows {
            return Err(backend_err(format!(
                "lunaris-embed-remote openai returned wrong shape: expected {expected_rows} rows, \
                 got {}",
                rows.len()
            )));
        }
        for (i, row) in rows.iter().enumerate() {
            if row.len() != self.dim {
                return Err(backend_err(format!(
                    "lunaris-embed-remote openai returned wrong shape: row {i} has dim {} \
                     (expected {})",
                    row.len(),
                    self.dim
                )));
            }
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedDatum>,
}

#[derive(Deserialize)]
struct EmbedDatum {
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

impl EmbedResponse {
    /// OpenAI returns rows tagged with `index`; sort by it so callers get rows
    /// aligned to their input order regardless of server ordering.
    fn into_ordered_rows(mut self) -> Vec<Vec<f32>> {
        self.data.sort_by_key(|d| d.index);
        self.data.into_iter().map(|d| d.embedding).collect()
    }
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>, LunarisError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        self.breaker.check()?;

        let url = self.endpoint();
        let mut last_err: Option<LunarisError> = None;
        for attempt in 0..MAX_ATTEMPTS {
            match self.try_once(&url, inputs).await {
                Ok(rows) => {
                    self.breaker.record_success();
                    return Ok(rows);
                }
                Err((false, e)) => {
                    // Fatal — do not retry, do not trip the breaker (this is a
                    // request/config fault, not an endpoint-down signal).
                    return Err(e);
                }
                Err((true, e)) => {
                    last_err = Some(e);
                    if attempt + 1 < MAX_ATTEMPTS {
                        let backoff = BACKOFF_BASE * 2u32.pow(attempt as u32);
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        }
        // Transient failures exhausted the retry budget → count against breaker.
        self.breaker.record_failure();
        Err(last_err
            .unwrap_or_else(|| backend_err("lunaris-embed-remote openai: retries exhausted")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_opens_after_threshold_then_half_opens() {
        let b = Breaker::new(3, Duration::from_millis(20));
        // Below threshold: stays closed.
        b.record_failure();
        b.record_failure();
        assert!(b.check().is_ok(), "closed below threshold");
        // Hit threshold: opens.
        b.record_failure();
        assert!(b.check().is_err(), "open at threshold");
        // After cooldown: half-open allows a trial.
        std::thread::sleep(Duration::from_millis(30));
        assert!(b.check().is_ok(), "half-open after cooldown");
    }

    #[test]
    fn breaker_success_resets_failures() {
        let b = Breaker::new(3, Duration::from_millis(20));
        b.record_failure();
        b.record_failure();
        b.record_success();
        b.record_failure();
        b.record_failure();
        // Only two failures since the reset → still closed.
        assert!(b.check().is_ok(), "reset clears the failure count");
    }

    #[test]
    fn new_rejects_empty_base_url() {
        let err = OpenAiEmbedder::new(OpenAiEmbedderOpts {
            base_url: "  ".to_string(),
            model: "m".to_string(),
            api_key: None,
            dim: 4,
        });
        assert!(err.is_err(), "empty base_url is rejected at construction");
    }

    #[test]
    fn opts_debug_redacts_api_key() {
        let opts = OpenAiEmbedderOpts {
            base_url: "http://x".to_string(),
            model: "m".to_string(),
            api_key: Some("sk-secret-value".to_string()),
            dim: 4,
        };
        let s = format!("{opts:?}");
        assert!(!s.contains("sk-secret-value"), "api key must be redacted in Debug");
        assert!(s.contains("***"), "redaction marker present");
    }

    #[test]
    fn response_rows_are_reordered_by_index() {
        let resp = EmbedResponse {
            data: vec![
                EmbedDatum { embedding: vec![2.0], index: 1 },
                EmbedDatum { embedding: vec![1.0], index: 0 },
            ],
        };
        assert_eq!(resp.into_ordered_rows(), vec![vec![1.0], vec![2.0]]);
    }

    fn test_embedder(base_url: &str) -> OpenAiEmbedder {
        OpenAiEmbedder::new(OpenAiEmbedderOpts {
            base_url: base_url.to_string(),
            model: "test-model".to_string(),
            api_key: Some("sk-test".to_string()),
            dim: 3,
        })
        .expect("client builds")
    }

    #[tokio::test]
    async fn embed_batch_happy_path_posts_and_parses() {
        use wiremock::matchers::{body_string_contains, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .and(header("authorization", "Bearer sk-test"))
            .and(body_string_contains("test-model"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"embedding": [0.1, 0.2, 0.3], "index": 0},
                    {"embedding": [0.4, 0.5, 0.6], "index": 1}
                ]
            })))
            .mount(&server)
            .await;

        let e = test_embedder(&server.uri());
        let rows = e.embed_batch(&["hello", "world"]).await.expect("embeds");
        assert_eq!(rows, vec![vec![0.1, 0.2, 0.3], vec![0.4, 0.5, 0.6]]);
    }

    #[tokio::test]
    async fn embed_batch_rejects_wrong_dim() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"embedding": [0.1, 0.2], "index": 0}] // dim 2, expected 3
            })))
            .mount(&server)
            .await;

        let e = test_embedder(&server.uri());
        let err = e.embed_batch(&["hello"]).await.expect_err("dim mismatch rejected");
        assert!(format!("{err}").contains("wrong shape"), "surfaces shape error: {err}");
    }

    #[tokio::test]
    async fn embed_batch_retries_transient_5xx_then_succeeds() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // First response: 503 (transient). up_to_n_times bounds this responder;
        // the later-mounted 200 responder serves the retry.
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"embedding": [0.1, 0.2, 0.3], "index": 0}]
            })))
            .mount(&server)
            .await;

        let e = test_embedder(&server.uri());
        let rows = e.embed_batch(&["hello"]).await.expect("succeeds after retry");
        assert_eq!(rows, vec![vec![0.1, 0.2, 0.3]]);
    }

    #[tokio::test]
    async fn embed_batch_4xx_is_fatal_not_retried() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Expect EXACTLY one call — a 400 must not be retried.
        Mock::given(method("POST"))
            .and(path("/v1/embeddings"))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)
            .mount(&server)
            .await;

        let e = test_embedder(&server.uri());
        let err = e.embed_batch(&["hello"]).await.expect_err("400 is fatal");
        assert!(format!("{err}").contains("HTTP 400"), "surfaces 400: {err}");
        // server drop asserts the .expect(1) — no retry happened.
    }

    #[tokio::test]
    async fn embed_batch_empty_input_short_circuits() {
        let e = test_embedder("http://127.0.0.1:1"); // unreachable — must not be hit
        assert!(e.embed_batch(&[]).await.expect("empty ok").is_empty());
    }
}

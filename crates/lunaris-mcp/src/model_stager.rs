//! Lazy GGUF model staging for `lunaris-mcp`.
//!
//! On first call to [`ensure_staged`]:
//! 1. Compute target path: `~/.lunaris/models/<filename>.gguf`.
//! 2. If file exists **and** sha256 matches expected → return path immediately.
//! 3. If file exists with wrong sha256 → warn on stderr, delete, re-download.
//! 4. Otherwise download from the canonical HuggingFace mirror URL, streaming
//!    bytes to `<target>.partial` with an [`indicatif`] progress bar on
//!    **STDERR** (stdout is the MCP JSON-RPC transport — writing to it silently
//!    disconnects Claude Code).
//! 5. Verify sha256 of the `.partial` file.
//! 6. Atomically rename `.partial` → target.
//!
//! # Lazy invariant
//!
//! `ensure_staged` is **never** called at MCP server start. Cold-start budget:
//! `tools/list` < 500 ms (Wave 3.2 gate). The first `memory.recall` (Wave 2.B)
//! pays the download cost, not the handshake.
//!
//! # Model sources (verified 2026-05-24 via HF API `/api/models/{repo}/tree/main`)
//!
//! | Kind | HF repo | Filename | SHA-256 (LFS oid) | Size |
//! |---|---|---|---|---|
//! | Embedder Q4_K_M | `mykor/granite-embedding-311m-multilingual-r2-GGUF` | `granite-embedding-311M-multilingual-r2-Q4_K_M.gguf` | `58d27f63…` | 253 MB |
//! | Reranker Q5_K_M | `gpustack/bge-reranker-v2-m3-GGUF` | `bge-reranker-v2-m3-Q5_K_M.gguf` | `1a212007…` | 468 MB |
//!
//! The file is saved under the locked name from the brief (dot-separated quant
//! suffix), NOT the mirror's hyphen-separated name. Wave 2 looks up by the
//! locked name; consistency beats provenance-mirroring.

#![allow(clippy::too_many_lines)]

use std::path::{Path, PathBuf};

use futures_util::StreamExt as _;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt as _};

// ── Model catalogue ───────────────────────────────────────────────────────────

/// The two GGUF models that `lunaris-mcp` can stage on first use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelKind {
    /// `ibm-granite/granite-embedding-311m-multilingual-r2` — Q4_K_M GGUF.
    ///
    /// Mirrored from `mykor/granite-embedding-311m-multilingual-r2-GGUF`
    /// (community conversion; upstream official repo has no GGUF artifacts).
    /// LFS sha256 verified via HF API on 2026-05-24.
    EmbedderGraniteQ4KM,

    /// `BAAI/bge-reranker-v2-m3` — Q5_K_M GGUF.
    ///
    /// Mirrored from `gpustack/bge-reranker-v2-m3-GGUF` (11k downloads;
    /// highest-quality community conversion). LFS sha256 verified via HF API
    /// on 2026-05-24.
    #[allow(dead_code)]
    RerankerBgeV2M3Q5KM,
}

impl ModelKind {
    /// Locked filename under `~/.lunaris/models/`.
    ///
    /// Uses the dot-separated quant suffix convention from the brief so Wave 2
    /// lookup via `LUNARIS_EMBEDDER_GGUF` / `LUNARIS_RERANKER_GGUF` is stable
    /// regardless of which mirror is used.
    pub(crate) fn filename(self) -> &'static str {
        match self {
            Self::EmbedderGraniteQ4KM => "granite-embedding-311m-multilingual-r2.Q4_K_M.gguf",
            Self::RerankerBgeV2M3Q5KM => "bge-reranker-v2-m3.Q5_K_M.gguf",
        }
    }

    /// Canonical HuggingFace `resolve/main/` URL for the GGUF file.
    ///
    /// These stable URLs redirect to HF's xet/CDN infrastructure. The `reqwest`
    /// client follows the redirect automatically (`redirect::Policy::limited(10)`
    /// is the default).
    ///
    /// To override for tests, call [`ensure_staged_with`] with a custom
    /// `base_url` pointing at a wiremock server.
    pub(crate) fn url(self) -> &'static str {
        match self {
            // https://huggingface.co/mykor/granite-embedding-311m-multilingual-r2-GGUF
            //   File: granite-embedding-311M-multilingual-r2-Q4_K_M.gguf (253 MB)
            //   LFS oid: 58d27f63e69ccf7abce27bf6b35bb0edebc3a1c05ad4a3165acaba1cdca107c0
            Self::EmbedderGraniteQ4KM => {
                "https://huggingface.co/mykor/granite-embedding-311m-multilingual-r2-GGUF\
                 /resolve/main/granite-embedding-311M-multilingual-r2-Q4_K_M.gguf"
            }
            // https://huggingface.co/gpustack/bge-reranker-v2-m3-GGUF
            //   File: bge-reranker-v2-m3-Q5_K_M.gguf (468 MB)
            //   LFS oid: 1a212007526c7083627eed92b39dd4472e90ff1374a03fb068733378220813ef
            Self::RerankerBgeV2M3Q5KM => {
                "https://huggingface.co/gpustack/bge-reranker-v2-m3-GGUF\
                 /resolve/main/bge-reranker-v2-m3-Q5_K_M.gguf"
            }
        }
    }

    /// Expected SHA-256 hex digest of the file (HF Git-LFS `oid sha256:HEX`).
    ///
    /// Sourced from `GET /api/models/{repo}/tree/main` on 2026-05-24.
    /// If this check fails on a freshly downloaded file the mirror has changed
    /// its content — treat as a security / integrity failure, not a bug.
    pub(crate) fn sha256(self) -> &'static str {
        match self {
            Self::EmbedderGraniteQ4KM => {
                "58d27f63e69ccf7abce27bf6b35bb0edebc3a1c05ad4a3165acaba1cdca107c0"
            }
            Self::RerankerBgeV2M3Q5KM => {
                "1a212007526c7083627eed92b39dd4472e90ff1374a03fb068733378220813ef"
            }
        }
    }

    /// Human-readable display name for progress bar messages.
    fn display_name(self) -> &'static str {
        match self {
            Self::EmbedderGraniteQ4KM => "granite-embedding-311m Q4_K_M",
            Self::RerankerBgeV2M3Q5KM => "bge-reranker-v2-m3 Q5_K_M",
        }
    }
}

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors returned by [`ensure_staged`] / [`ensure_staged_with`].
#[derive(Debug, Error)]
pub(crate) enum StageError {
    /// `dirs::home_dir()` returned `None` (no `HOME` env on the current OS).
    #[error("could not resolve home directory — set $HOME")]
    NoHome,

    /// I/O failure creating the models directory, reading or renaming files.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP error from `reqwest` (connection refused, DNS failure, 4xx/5xx).
    #[error("http download failed: {0}")]
    Http(#[from] reqwest::Error),

    /// SHA-256 of the downloaded file does not match the hardcoded expected
    /// digest. The partial file has already been deleted before this error
    /// surfaces; retrying `ensure_staged` will attempt a fresh download.
    #[error(
        "sha256 mismatch for {filename}: expected {expected}, got {got}; \
         partial file deleted — re-run to retry download"
    )]
    ShaMismatch { filename: String, expected: String, got: String },
}

// ── RAII partial-file guard ───────────────────────────────────────────────────

/// Holds a `.partial` file path and best-effort deletes it on `Drop`.
///
/// Consumed by [`PartialGuard::commit`] (which renames to the final path) so
/// the delete is skipped on the happy path. On any early return — error,
/// panic, or future cancellation — the `.partial` file is cleaned up.
struct PartialGuard {
    partial: PathBuf,
    committed: bool,
}

impl PartialGuard {
    fn new(partial: PathBuf) -> Self {
        Self { partial, committed: false }
    }

    /// Atomically rename `.partial` → `target`, consuming the guard.
    async fn commit(mut self, target: &Path) -> Result<(), std::io::Error> {
        fs::rename(&self.partial, target).await?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartialGuard {
    fn drop(&mut self) {
        if !self.committed {
            // Best-effort; ignore `NotFound` (already cleaned up) and all
            // other errors — we are inside `Drop`, cannot propagate.
            let _ = std::fs::remove_file(&self.partial);
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Ensure `kind` is staged at `~/.lunaris/models/<filename>.gguf`.
///
/// Returns the path to the verified GGUF file.
///
/// # Behaviour
///
/// - **Already staged, sha matches** → returns immediately (no I/O except stat
///   + read for sha).
/// - **Already staged, sha mismatch** → emits a `tracing::warn!` to stderr,
///   deletes the corrupt file, re-downloads.
/// - **Not yet staged** → downloads with an indicatif progress bar on **stderr**
///   (never stdout — stdout is the MCP transport).
///
/// # Lazy invariant
///
/// Never called at MCP server start. Only the `memory.recall` handler (Wave
/// 2.B) calls this. Cold-start budget gate (Wave 3.2) asserts `tools/list` <
/// 500 ms; this function is the reason that budget is met.
pub(crate) async fn ensure_staged(kind: ModelKind) -> Result<PathBuf, StageError> {
    let dir = models_dir()?;
    ensure_staged_with(kind, dir, kind.url(), kind.sha256()).await
}

/// Testable variant of [`ensure_staged`] that accepts an explicit `models_dir`,
/// `base_url`, and `expected_sha`.
///
/// Tests pass a `wiremock` server URI as `base_url` and the SHA-256 of their
/// synthetic payload as `expected_sha`, so no real network calls are made and
/// sha verification is exercised end-to-end against realistic data. Mirrors the
/// `scope_resolver::resolve_with` injection-point pattern.
pub(crate) async fn ensure_staged_with(
    kind: ModelKind,
    models_dir: PathBuf,
    base_url: &str,
    expected_sha: &str,
) -> Result<PathBuf, StageError> {
    let target = models_dir.join(kind.filename());

    // 1. If file already exists, verify sha.
    if target.exists() {
        match verify_sha256(&target, expected_sha).await? {
            true => {
                tracing::debug!(
                    path = %target.display(),
                    "model already staged and sha256 verified"
                );
                return Ok(target);
            }
            false => {
                tracing::warn!(
                    path = %target.display(),
                    "sha256 mismatch on existing model file — deleting and re-downloading"
                );
                fs::remove_file(&target).await?;
            }
        }
    }

    // 2. Create models dir if needed.
    fs::create_dir_all(&models_dir).await?;

    // 3. Build the full download URL.
    //    `base_url` is either `kind.url()` (production) or a wiremock URI
    //    (tests). For tests the wiremock mock is registered at the path
    //    `/<filename>`, so we append just the filename.
    let filename = kind.filename();
    let url = if base_url == kind.url() {
        // Production: base_url IS the full URL already.
        base_url.to_string()
    } else {
        // Test injection: base_url is the mock server root; append filename.
        format!("{base_url}/{filename}")
    };

    // 4. Download to <target>.partial with progress bar on stderr.
    let partial_path = models_dir.join(format!("{filename}.partial"));
    let guard = PartialGuard::new(partial_path.clone());
    download_with_progress(&url, &partial_path, kind.display_name()).await?;

    // 5. Verify sha256 of the downloaded partial.
    let got_sha = compute_sha256(&partial_path).await?;
    if got_sha != expected_sha {
        // Guard's Drop will delete the partial file.
        return Err(StageError::ShaMismatch {
            filename: filename.to_string(),
            expected: expected_sha.to_string(),
            got: got_sha,
        });
    }

    // 6. Atomically rename partial → target.
    guard.commit(&target).await?;

    tracing::info!(
        path = %target.display(),
        "model staged successfully"
    );
    Ok(target)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Resolve the GGUF models directory.
///
/// Resolution order:
///
/// 1. `LUNARIS_MCP_MODELS_DIR` env var — used by integration tests to redirect
///    staging away from `~/.lunaris/models/` so tests are hermetic and don't
///    accidentally download or read real GGUF files. When set, this value is
///    returned as-is (the caller is responsible for creating the directory).
/// 2. `~/.lunaris/models/` — the production default resolved via
///    [`dirs::home_dir`]. Returns [`StageError::NoHome`] if `$HOME` is unset.
///
/// # Test isolation
///
/// Set `LUNARIS_MCP_MODELS_DIR` to a per-test temp directory to prevent
/// cross-test contamination. The Wave 3.2 cold-start gate relies on this to
/// assert that no `*.gguf` appears under the controlled models directory after
/// `tools/list` — proving the lazy embedder invariant holds.
fn models_dir() -> Result<PathBuf, StageError> {
    if let Ok(dir) = std::env::var("LUNARIS_MCP_MODELS_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    let home = dirs::home_dir().ok_or(StageError::NoHome)?;
    Ok(home.join(".lunaris").join("models"))
}

/// Verify the SHA-256 hex digest of `path` against `expected_hex`.
///
/// Reads the file in 64 KiB chunks to avoid loading large GGUFs into memory.
/// Returns `true` if the digest matches, `false` if it does not.
async fn verify_sha256(path: &Path, expected_hex: &str) -> Result<bool, StageError> {
    let got = compute_sha256(path).await?;
    Ok(got == expected_hex)
}

/// Compute the SHA-256 hex digest of `path` using 64 KiB read chunks.
async fn compute_sha256(path: &Path) -> Result<String, StageError> {
    use tokio::io::AsyncReadExt as _;

    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 65_536];

    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Stream `url` to `partial_path`, displaying an indicatif progress bar on
/// **stderr**.
///
/// # CRITICAL: stderr only
///
/// The progress bar MUST use `ProgressDrawTarget::stderr()`. stdout is the
/// MCP JSON-RPC framing transport — writing any bytes to stdout corrupts the
/// Content-Length framing and causes Claude Code to silently disconnect.
async fn download_with_progress(
    url: &str,
    partial_path: &Path,
    display_name: &str,
) -> Result<(), StageError> {
    let client = reqwest::Client::new();
    let response = client.get(url).send().await?.error_for_status()?;
    let content_length = response.content_length();

    // ── Progress bar — ALWAYS targets stderr ─────────────────────────────────
    let pb = match content_length {
        Some(len) => {
            let pb = ProgressBar::with_draw_target(
                Some(len),
                // INVARIANT: stderr only. Stdout is the MCP transport.
                ProgressDrawTarget::stderr(),
            );
            pb.set_style(
                ProgressStyle::with_template(
                    "{msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=> "),
            );
            pb
        }
        None => {
            let pb = ProgressBar::with_draw_target(
                None,
                // INVARIANT: stderr only. Stdout is the MCP transport.
                ProgressDrawTarget::stderr(),
            );
            pb.set_style(
                ProgressStyle::with_template("{msg} {spinner} {bytes}")
                    .unwrap_or_else(|_| ProgressStyle::default_spinner()),
            );
            pb
        }
    };
    pb.set_message(format!("downloading {display_name}"));

    // ── Stream response bytes → file ──────────────────────────────────────────
    let mut file = fs::File::create(partial_path).await?;
    let mut stream = response.bytes_stream();

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result?;
        file.write_all(&chunk).await?;
        pb.inc(chunk.len() as u64);
    }
    file.flush().await?;

    pb.finish_with_message(format!("{display_name} downloaded"));
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    // ── Fixtures ──────────────────────────────────────────────────────────────

    /// 200 KB synthetic GGUF payload — small enough for fast in-process tests.
    /// The "GGUF" magic header is set so any future format check passes.
    fn fake_gguf() -> Vec<u8> {
        let mut v = vec![0u8; 200_000];
        v[..4].copy_from_slice(b"GGUF");
        v
    }

    /// Compute the SHA-256 hex of a byte slice (synchronous helper for tests).
    fn sha256_of(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    /// Returns [`ModelKind::EmbedderGraniteQ4KM`] as the default kind for tests.
    ///
    /// Tests pass `sha256_of(&fake_gguf())` as `expected_sha` to
    /// [`ensure_staged_with`], so the production SHA constant in
    /// [`ModelKind::sha256`] is never consulted during test runs.
    fn embedder_kind() -> ModelKind {
        ModelKind::EmbedderGraniteQ4KM
    }

    // ── Test 1: downloads on first call, idempotent on second ────────────────

    /// Calls `ensure_staged_with` twice against a mock that serves the file
    /// exactly once. The second call must hit the "sha matches → return
    /// immediately" branch without contacting the server.
    #[tokio::test]
    async fn downloads_on_first_call_and_is_idempotent() {
        let td = TempDir::new().unwrap();
        let models_dir = td.path().to_path_buf();
        let payload = fake_gguf();
        let expected_sha = sha256_of(&payload);

        let kind = embedder_kind();
        let filename = kind.filename();

        // Mock serves the file exactly once — the second call must NOT hit it.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{filename}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
            .expect(1)
            .mount(&server)
            .await;

        // First call: file absent → download → sha verify → atomic rename.
        let path1 = ensure_staged_with(kind, models_dir.clone(), &server.uri(), &expected_sha)
            .await
            .expect("first ensure_staged_with must succeed");

        assert!(path1.exists(), "staged file must exist after first call");
        let on_disk = std::fs::read(&path1).unwrap();
        assert_eq!(on_disk, payload, "staged file must contain the served payload");

        // Second call: file present + sha matches → return immediately, no download.
        let path2 = ensure_staged_with(kind, models_dir.clone(), &server.uri(), &expected_sha)
            .await
            .expect("second ensure_staged_with must succeed (idempotent)");

        assert_eq!(path1, path2, "both calls must return the same path");

        // wiremock assertion: exactly 1 GET was made across both calls.
        server.verify().await;
    }

    // ── Test 2: pre-seeded file → zero downloads (idempotent no-op) ──────────

    /// Pre-seeds the target file with the correct payload before the first call.
    /// `ensure_staged_with` must detect sha match and return without ever
    /// contacting the mock server (`.expect(0)`).
    #[tokio::test]
    async fn pre_seeded_file_skips_download() {
        let td = TempDir::new().unwrap();
        let models_dir = td.path().to_path_buf();
        let payload = fake_gguf();
        let expected_sha = sha256_of(&payload);

        let kind = embedder_kind();
        let filename = kind.filename();
        let target = models_dir.join(filename);

        // Pre-seed the correctly-hashed file on disk.
        std::fs::write(&target, &payload).unwrap();

        // Mock expects ZERO network calls — pre-seeded + sha match → no download.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{filename}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
            .expect(0)
            .mount(&server)
            .await;

        let result_path =
            ensure_staged_with(kind, models_dir.clone(), &server.uri(), &expected_sha)
                .await
                .expect("ensure_staged_with must succeed with pre-seeded file");

        assert_eq!(result_path, target, "returned path must be the pre-seeded target");

        // Zero GETs issued.
        server.verify().await;
    }

    // ── Test 3: sha mismatch triggers re-download ─────────────────────────────

    /// Pre-seeds the target path with corrupt bytes so the sha check fails.
    /// `ensure_staged_with` must delete the corrupt file, re-download the
    /// correct payload, and return success. Mock `.expect(1)`.
    #[tokio::test]
    async fn sha_mismatch_triggers_redownload() {
        let td = TempDir::new().unwrap();
        let models_dir = td.path().to_path_buf();
        let payload = fake_gguf();
        let expected_sha = sha256_of(&payload);

        let kind = embedder_kind();
        let filename = kind.filename();
        let target = models_dir.join(filename);

        // Seed a corrupt file (sha will not match).
        std::fs::write(&target, b"corrupt garbage - sha mismatch bait").unwrap();

        // Mock serves the correct payload exactly once (the re-download).
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{filename}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
            .expect(1)
            .mount(&server)
            .await;

        let result_path =
            ensure_staged_with(kind, models_dir.clone(), &server.uri(), &expected_sha)
                .await
                .expect("ensure_staged_with must succeed after re-downloading");

        // Target must contain the fresh payload, not the corrupt bytes.
        let on_disk = std::fs::read(&result_path).unwrap();
        assert_eq!(on_disk, payload, "re-downloaded file must match served payload");

        // Exactly one GET was made (the re-download after sha mismatch).
        server.verify().await;
    }

    // ── Test 4: partial file cleaned up on HTTP failure ───────────────────────

    /// Calls `ensure_staged_with` against a mock that always returns 500.
    /// The call must return `Err(StageError::Http(_))`, and both the `.partial`
    /// and the final target must be absent after the failure.
    #[tokio::test]
    async fn partial_file_cleaned_up_on_failure() {
        let td = TempDir::new().unwrap();
        let models_dir = td.path().to_path_buf();

        let kind = embedder_kind();
        let filename = kind.filename();
        let partial_path = models_dir.join(format!("{filename}.partial"));
        let target_path = models_dir.join(filename);

        // Mock always returns 500 — reqwest's `error_for_status()` propagates Err.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{filename}")))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        // Use a sha that can never match so we'd only succeed on happy path.
        let result = ensure_staged_with(
            kind,
            models_dir.clone(),
            &server.uri(),
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .await;

        assert!(
            matches!(result, Err(StageError::Http(_))),
            "HTTP 500 must propagate as StageError::Http, got: {result:?}"
        );
        assert!(!partial_path.exists(), ".partial file must be absent after failed download");
        assert!(!target_path.exists(), "target file must be absent after failed download");

        server.verify().await;
    }

    // ── Test 5: progress bar targets stderr, never stdout ────────────────────

    /// Code-inspection test: assert the `download_with_progress` source uses
    /// `ProgressDrawTarget::stderr()` and never `ProgressDrawTarget::stdout()`.
    ///
    /// This is the lightest-weight approach that catches the real regression
    /// (someone changing the draw target). No subprocess spawning needed; no
    /// extra deps needed. Accepted by the brief as the "code-review the
    /// construct site" fallback.
    #[test]
    fn progress_bar_never_targets_stdout() {
        let src = include_str!("model_stager.rs");

        // Every ProgressDrawTarget call in the production code (outside this
        // test module) must use stderr().
        assert!(
            src.contains("ProgressDrawTarget::stderr()"),
            "download_with_progress must use ProgressDrawTarget::stderr()"
        );

        // Count occurrences of "ProgressDrawTarget::stderr()" in production
        // code (outside the test module). We detect the test boundary by
        // splitting on the "#[cfg(test)]" marker and checking the FIRST half.
        let production_half = src.split("#[cfg(test)]").next().unwrap_or(src);

        // The production half must not reference the stdout variant at all.
        // We search for the suffix "::stdout()" to avoid matching this very
        // assertion string — "stdout" alone would appear in comments.
        let stdout_variant = "ProgressDrawTarget::stdout()";
        assert!(
            !production_half.contains(stdout_variant),
            "production code must NEVER use ProgressDrawTarget::stdout() — \
             stdout is the MCP JSON-RPC transport and polluting it disconnects \
             Claude Code silently"
        );
    }

    // ── Test 6: partial RAII guard deletes on drop ────────────────────────────

    #[tokio::test]
    async fn partial_guard_deletes_on_drop_without_commit() {
        let td = TempDir::new().unwrap();
        let partial = td.path().join("model.gguf.partial");

        // Create the file.
        fs::write(&partial, b"bytes").await.unwrap();
        assert!(partial.exists());

        // Drop the guard without committing.
        let guard = PartialGuard::new(partial.clone());
        drop(guard);

        assert!(!partial.exists(), "PartialGuard::drop must delete the partial file");
    }

    #[tokio::test]
    async fn partial_guard_keeps_file_on_commit() {
        let td = TempDir::new().unwrap();
        let partial = td.path().join("model.gguf.partial");
        let final_path = td.path().join("model.gguf");

        fs::write(&partial, b"bytes").await.unwrap();

        let guard = PartialGuard::new(partial.clone());
        guard.commit(&final_path).await.unwrap();

        assert!(!partial.exists(), "partial must be gone after commit");
        assert!(final_path.exists(), "target must exist after commit");
    }
}

//! Download-and-verify for the GGUF models named in [`crate::models`].
//!
//! Behind the optional `model-staging` feature, because it pulls HTTP, TLS,
//! SHA-256 and a progress bar. The *catalogue* — which model, which digest,
//! which path — is unconditional and lives in [`crate::models`], so a crate
//! that only needs to name a model never compiles any of this.
//!
//! # Flow
//!
//! 1. Resolve the target: `~/.lunaris/models/<locked filename>`.
//! 2. Present and digest matches → return immediately.
//! 3. Present and digest differs → warn, delete, re-download.
//! 4. Stream the download into `<target>.partial` behind an RAII guard, so an
//!    error, a panic or a Ctrl-C can never leave bytes that look staged.
//! 5. Verify the digest of the partial, then atomically rename into place.
//!
//! # The progress bar draws on stderr. Always.
//!
//! Both callers have a stdout that means something: for `lunaris-mcp` it is
//! the JSON-RPC framing transport (a stray byte silently disconnects Claude
//! Code), and for `lunaris-cli` it is the command's result, which scripts
//! pipe. `ProgressDrawTarget::stderr()` is an invariant, and
//! `progress_never_draws_on_stdout` below pins it.
//!
//! # Laziness
//!
//! Nothing here runs at process start. `lunaris-mcp` stages on the first
//! recall (its cold-start gate asserts `tools/list` stays under 500 ms and
//! that no `*.gguf` appears), and `lunaris try` stages when the user has
//! already asked for a trial run.

use std::path::{Path, PathBuf};

use futures_util::StreamExt as _;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt as _};

use crate::models::{ModelKind, models_dir};

/// Bound the handshake with the mirror.
///
/// A stalled CDN is the most likely failure on a first run, and an unbounded
/// hang is indistinguishable from a slow link. The body itself is deliberately
/// *not* bounded: a slow 253 MB download is legitimate.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// What [`ensure_model`] did, so the caller can say so out loud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Staged {
    /// An explicit env override was honoured untouched.
    OperatorSupplied,
    /// Already on disk, and its digest matched.
    AlreadyPresent,
    /// Downloaded during this call.
    Downloaded,
}

/// Errors returned by the staging entry points.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StageError {
    /// Neither `$HOME` nor [`dirs::home_dir`] resolved.
    #[error("could not resolve home directory — set $HOME")]
    NoHome,

    /// The operator named a file that is not there.
    ///
    /// Deliberately **not** a silent fall-through to downloading the default:
    /// they named a path, and quietly running different weights under the same
    /// name is how a benchmark result stops meaning anything.
    #[error(
        "{env_var} points at {path}, which does not exist.\n\
         Fix the path, or unset the variable to stage the default {model} \
         under ~/.lunaris/models/."
    )]
    OverrideMissing { env_var: &'static str, path: PathBuf, model: &'static str },

    /// I/O failure creating the models directory, reading or renaming files.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Transport failure — DNS, connection refused, a 4xx/5xx from the mirror.
    #[error("http download failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The digest of the downloaded bytes does not match the pinned one.
    ///
    /// The partial file is already deleted when this surfaces. This is an
    /// integrity failure, not a bug: the mirror served different content.
    #[error(
        "sha256 mismatch for {filename}: expected {expected}, got {got}; \
         partial file deleted — re-run to retry download"
    )]
    ShaMismatch { filename: String, expected: String, got: String },
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Ensure `kind` is usable, honouring the operator's env override.
///
/// This is the front door. Resolution order:
///
/// 1. [`ModelKind::env_override`] naming an existing file → returned untouched
///    as [`Staged::OperatorSupplied`]; nothing is downloaded.
/// 2. That variable set but naming a missing file →
///    [`StageError::OverrideMissing`]. The engine's own lookup warns and falls
///    through to a no-op embedder here, which is silent and produces empty
///    results; saying so is strictly better than staging 253 MB the engine
///    will then ignore.
/// 3. Otherwise → [`ensure_staged`], reporting whether it downloaded.
pub async fn ensure_model(kind: ModelKind) -> Result<(PathBuf, Staged), StageError> {
    let env_var = kind.env_override();
    if let Some(p) =
        std::env::var_os(env_var).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
    {
        if p.is_file() {
            return Ok((p, Staged::OperatorSupplied));
        }
        return Err(StageError::OverrideMissing { env_var, path: p, model: kind.display_name() });
    }

    let dir = models_dir().ok_or(StageError::NoHome)?;
    ensure_staged_with(kind, dir, kind.url(), kind.sha256()).await
}

/// Ensure `kind` is staged under [`crate::models::models_dir`].
///
/// Ignores the env override — callers that want it use [`ensure_model`].
pub async fn ensure_staged(kind: ModelKind) -> Result<PathBuf, StageError> {
    let dir = models_dir().ok_or(StageError::NoHome)?;
    ensure_staged_with(kind, dir, kind.url(), kind.sha256()).await.map(|(p, _)| p)
}

/// Testable variant of [`ensure_staged`] taking an explicit `models_dir`,
/// `base_url` and `expected_sha`.
///
/// Tests pass a `wiremock` server URI as `base_url` and the digest of their
/// synthetic payload as `expected_sha`, so no real network call is made and
/// digest verification is still exercised end to end.
pub async fn ensure_staged_with(
    kind: ModelKind,
    models_dir: PathBuf,
    base_url: &str,
    expected_sha: &str,
) -> Result<(PathBuf, Staged), StageError> {
    let filename = kind.filename();
    let target = models_dir.join(filename);

    // 1. Present already? Trust it only if the digest agrees.
    if target.exists() {
        if compute_sha256(&target).await? == expected_sha {
            tracing::debug!(path = %target.display(), "model already staged and sha256 verified");
            return Ok((target, Staged::AlreadyPresent));
        }
        tracing::warn!(
            path = %target.display(),
            "sha256 mismatch on existing model file — deleting and re-downloading"
        );
        fs::remove_file(&target).await?;
    }

    fs::create_dir_all(&models_dir).await?;

    // 2. `base_url` is either the catalogue URL (production, already whole) or
    //    a mock server root (tests, where the mock is mounted at `/<filename>`).
    let url = if base_url == kind.url() {
        base_url.to_string()
    } else {
        format!("{base_url}/{filename}")
    };

    // 3. Stream into `<target>.partial` behind the RAII guard.
    let partial_path = models_dir.join(format!("{filename}.partial"));
    let guard = PartialGuard::new(partial_path.clone());
    download_with_progress(&url, &partial_path, kind).await?;

    // 4. Verify before anything is allowed to look staged.
    let got_sha = compute_sha256(&partial_path).await?;
    if got_sha != expected_sha {
        // The guard's Drop deletes the partial.
        return Err(StageError::ShaMismatch {
            filename: filename.to_string(),
            expected: expected_sha.to_string(),
            got: got_sha,
        });
    }

    guard.commit(&target).await?;
    tracing::info!(path = %target.display(), "model staged successfully");
    Ok((target, Staged::Downloaded))
}

// ── Internals ─────────────────────────────────────────────────────────────────

/// Deletes the `.partial` file unless [`PartialGuard::commit`] consumed it.
struct PartialGuard {
    partial: PathBuf,
    committed: bool,
}

impl PartialGuard {
    fn new(partial: PathBuf) -> Self {
        Self { partial, committed: false }
    }

    /// Atomically rename `.partial` → `target`, consuming the guard.
    async fn commit(mut self, target: &Path) -> std::io::Result<()> {
        fs::rename(&self.partial, target).await?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartialGuard {
    fn drop(&mut self) {
        if !self.committed {
            // Best-effort: we are inside Drop and cannot propagate. `NotFound`
            // is the common case (the download never created the file).
            let _ = std::fs::remove_file(&self.partial);
        }
    }
}

/// SHA-256 hex digest of `path`, read in 64 KiB chunks so a 468 MB GGUF never
/// lands in memory.
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

/// Stream `url` into `partial_path`, drawing a bar on **stderr**.
///
/// # CRITICAL: stderr only
///
/// stdout is the MCP JSON-RPC transport in one caller and the command's result
/// in the other. Bytes written there corrupt both.
async fn download_with_progress(
    url: &str,
    partial_path: &Path,
    kind: ModelKind,
) -> Result<(), StageError> {
    let client = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT).build()?;
    let response = client.get(url).send().await?.error_for_status()?;

    let pb = match response.content_length() {
        // INVARIANT: stderr. Never stdout.
        Some(len) => {
            let pb = ProgressBar::with_draw_target(Some(len), ProgressDrawTarget::stderr());
            pb.set_style(
                ProgressStyle::with_template(
                    "  {msg} [{bar:32.cyan/blue}] {bytes}/{total_bytes} ({eta})",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=> "),
            );
            pb
        }
        None => {
            let pb = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr());
            pb.set_style(
                ProgressStyle::with_template("  {msg} {spinner} {bytes}")
                    .unwrap_or_else(|_| ProgressStyle::default_spinner()),
            );
            pb
        }
    };
    pb.set_message(format!("downloading {} ({} MB)", kind.display_name(), kind.size_mb()));

    let mut file = fs::File::create(partial_path).await?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        pb.inc(chunk.len() as u64);
    }
    file.flush().await?;
    pb.finish_with_message(format!("{} staged", kind.display_name()));
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

    /// 200 KB synthetic GGUF payload — small enough for fast in-process tests.
    /// The "GGUF" magic header is set so any future format check passes.
    fn fake_gguf() -> Vec<u8> {
        let mut v = vec![0u8; 200_000];
        v[..4].copy_from_slice(b"GGUF");
        v
    }

    fn sha256_of(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    const KIND: ModelKind = ModelKind::EmbedderGraniteQ4KM;

    /// Two calls, a mock that serves the file exactly once. The second call
    /// must take the "digest matches → return" branch without a request.
    #[tokio::test]
    async fn downloads_on_first_call_and_is_idempotent() {
        let td = TempDir::new().unwrap();
        let dir = td.path().to_path_buf();
        let payload = fake_gguf();
        let sha = sha256_of(&payload);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{}", KIND.filename())))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
            .expect(1)
            .mount(&server)
            .await;

        let (p1, how1) = ensure_staged_with(KIND, dir.clone(), &server.uri(), &sha).await.unwrap();
        assert_eq!(std::fs::read(&p1).unwrap(), payload, "staged bytes must be the served bytes");
        assert_eq!(how1, Staged::Downloaded, "the first call fetched the file");

        let (p2, how2) = ensure_staged_with(KIND, dir, &server.uri(), &sha).await.unwrap();
        assert_eq!(p1, p2);
        assert_eq!(
            how2,
            Staged::AlreadyPresent,
            "the second call must report reuse, not a download it did not perform"
        );

        server.verify().await;
    }

    /// Pre-seeded and correct → zero requests.
    #[tokio::test]
    async fn pre_seeded_file_skips_download() {
        let td = TempDir::new().unwrap();
        let dir = td.path().to_path_buf();
        let payload = fake_gguf();
        let sha = sha256_of(&payload);
        let target = dir.join(KIND.filename());
        std::fs::write(&target, &payload).unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{}", KIND.filename())))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(payload))
            .expect(0)
            .mount(&server)
            .await;

        let (got, how) = ensure_staged_with(KIND, dir, &server.uri(), &sha).await.unwrap();
        assert_eq!(got, target);
        assert_eq!(how, Staged::AlreadyPresent);
        server.verify().await;
    }

    /// A present-but-corrupt file is deleted and re-fetched, not trusted.
    #[tokio::test]
    async fn sha_mismatch_triggers_redownload() {
        let td = TempDir::new().unwrap();
        let dir = td.path().to_path_buf();
        let payload = fake_gguf();
        let sha = sha256_of(&payload);
        let target = dir.join(KIND.filename());
        std::fs::write(&target, b"corrupt garbage - sha mismatch bait").unwrap();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{}", KIND.filename())))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
            .expect(1)
            .mount(&server)
            .await;

        let (got, how) = ensure_staged_with(KIND, dir, &server.uri(), &sha).await.unwrap();
        assert_eq!(std::fs::read(&got).unwrap(), payload);
        assert_eq!(how, Staged::Downloaded, "a corrupt file is re-fetched, not reused");
        server.verify().await;
    }

    /// Served bytes that do not match the pin are rejected and leave nothing
    /// behind — the integrity check, not just the transport check.
    #[tokio::test]
    async fn served_bytes_that_miss_the_pin_stage_nothing() {
        let td = TempDir::new().unwrap();
        let dir = td.path().to_path_buf();
        let target = dir.join(KIND.filename());
        let partial = dir.join(format!("{}.partial", KIND.filename()));

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{}", KIND.filename())))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(b"not the pinned model".to_vec()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let err = ensure_staged_with(KIND, dir, &server.uri(), &sha256_of(&fake_gguf()))
            .await
            .expect_err("a digest mismatch must not be staged");
        assert!(matches!(err, StageError::ShaMismatch { .. }), "got {err:?}");
        assert!(!target.exists(), "a file that failed its digest must never appear as staged");
        assert!(!partial.exists(), "the partial must be cleaned up");
        server.verify().await;
    }

    /// A 500 propagates and leaves neither the partial nor the target.
    #[tokio::test]
    async fn partial_file_cleaned_up_on_failure() {
        let td = TempDir::new().unwrap();
        let dir = td.path().to_path_buf();
        let partial = dir.join(format!("{}.partial", KIND.filename()));
        let target = dir.join(KIND.filename());

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{}", KIND.filename())))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let result = ensure_staged_with(
            KIND,
            dir,
            &server.uri(),
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .await;

        assert!(matches!(result, Err(StageError::Http(_))), "got {result:?}");
        assert!(!partial.exists());
        assert!(!target.exists());
        server.verify().await;
    }

    /// Code-inspection guard on the one invariant a runtime test cannot see
    /// without capturing a real process's stdout.
    #[test]
    fn progress_never_draws_on_stdout() {
        let src = include_str!("model_staging.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(
            production.contains("ProgressDrawTarget::stderr()"),
            "the download path must draw on stderr"
        );
        assert!(
            !production.contains(concat!("ProgressDrawTarget", "::stdout()")),
            "stdout is the MCP JSON-RPC transport in one caller and the command's result in \
             the other — a progress bar there corrupts both, silently"
        );
    }

    #[tokio::test]
    async fn an_uncommitted_partial_never_survives() {
        let td = TempDir::new().unwrap();
        let partial = td.path().join("model.gguf.partial");
        fs::write(&partial, b"half a model").await.unwrap();
        drop(PartialGuard::new(partial.clone()));
        assert!(!partial.exists());
    }

    #[tokio::test]
    async fn a_committed_partial_becomes_the_target() {
        let td = TempDir::new().unwrap();
        let partial = td.path().join("model.gguf.partial");
        let target = td.path().join("model.gguf");
        fs::write(&partial, b"a whole model").await.unwrap();

        PartialGuard::new(partial.clone()).commit(&target).await.unwrap();
        assert!(!partial.exists());
        assert!(target.exists());
    }
}

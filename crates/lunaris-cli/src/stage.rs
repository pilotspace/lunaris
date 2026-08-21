//! Stage the embedder GGUF for `lunaris try`, with a visible progress bar.
//!
//! # Why this file exists twice
//!
//! `lunaris-mcp/src/model_stager.rs` already does this, and does it well — but
//! it is `pub(crate)`, so no other crate can call it. That is exactly the gap
//! W0.7 exists to close ("promote `model_stager` into `lunaris-core`"), and it
//! is the same gap that leaves `pip install lunaris` users with silent empty
//! results. The front door cannot wait for that promotion, and it also cannot
//! tell a stranger to go fetch a 253 MB file by hand.
//!
//! So this is a deliberate, temporary second copy, and it is pinned to the
//! first: `tests/stage_contract.rs` reads the mcp source and asserts both name
//! the same URL and the same SHA-256. When W0.7 lands, both copies delete and
//! the callers point at `lunaris_core`.
//!
//! # Contract
//!
//! * Target is `$HOME/.lunaris/models/<filename>` — byte-identical to what
//!   `lunaris::handle::llamacpp_gguf_path` looks for, because staging to a path
//!   the engine does not consult is the worst of both worlds.
//! * `LUNARIS_EMBEDDER_GGUF` pointing at an existing file short-circuits
//!   everything: an operator who staged their own weights is never second-guessed.
//! * Downloads stream to `<target>.partial` and are renamed only after the
//!   SHA-256 matches, so an interrupted download can never be mistaken for a
//!   staged model.
//! * The progress bar draws on **stderr**. stdout is the command's result.

use std::path::{Path, PathBuf};

use futures_util::StreamExt as _;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

/// Filename under `~/.lunaris/models/`. MUST match
/// `lunaris::handle::LLAMACPP_EMBEDDER_GGUF`, which is what the engine looks
/// for when `LUNARIS_EMBEDDER_GGUF` is unset.
pub(crate) const EMBEDDER_FILENAME: &str = "granite-embedding-311m-multilingual-r2.Q4_K_M.gguf";

/// Canonical mirror. Same repo lunaris-mcp uses; pinned by `stage_contract.rs`.
pub(crate) const EMBEDDER_URL: &str = "https://huggingface.co/mykor/granite-embedding-311m-multilingual-r2-GGUF/resolve/main/granite-embedding-311M-multilingual-r2-Q4_K_M.gguf";

/// Git-LFS `oid sha256` of the file above. A mismatch on freshly downloaded
/// bytes is an integrity failure, not a bug — the mirror changed content.
pub(crate) const EMBEDDER_SHA256: &str =
    "58d27f63e69ccf7abce27bf6b35bb0edebc3a1c05ad4a3165acaba1cdca107c0";

/// Approximate download size, used only to set expectations in prose before the
/// bar appears. A number the user sees before the wait is what turns a hang
/// into a wait.
pub(crate) const EMBEDDER_MB: u64 = 253;

/// Where the engine looks. Resolved from `$HOME` (not `dirs::home_dir()`)
/// because `llamacpp_gguf_path` reads `$HOME` — resolving it differently here
/// would stage to a path the engine then ignores.
fn models_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .ok_or_else(|| anyhow::anyhow!("cannot resolve $HOME to find ~/.lunaris/models"))?;
    Ok(home.join(".lunaris").join("models"))
}

/// What [`ensure_embedder`] did, so the caller can say so out loud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Staged {
    /// An explicit `LUNARIS_EMBEDDER_GGUF` was honoured untouched.
    OperatorSupplied,
    /// Already on disk and verified.
    AlreadyPresent,
    /// Downloaded during this run.
    Downloaded,
}

/// Ensure the embedder weights are on disk where the engine will find them.
///
/// Returns the resolved path and how it got there.
pub(crate) async fn ensure_embedder() -> anyhow::Result<(PathBuf, Staged)> {
    if let Some(p) = std::env::var_os("LUNARIS_EMBEDDER_GGUF")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
    {
        if p.is_file() {
            return Ok((p, Staged::OperatorSupplied));
        }
        // Do NOT silently fall back to downloading: the operator named a path,
        // and quietly using a different model is how a benchmark result stops
        // meaning anything.
        anyhow::bail!(
            "LUNARIS_EMBEDDER_GGUF points at {}, which does not exist.\n\
             Fix the path, or unset the variable to let `lunaris try` stage the \
             default granite-embedding-311m GGUF under ~/.lunaris/models/.",
            p.display()
        );
    }

    let dir = models_dir()?;
    let target = dir.join(EMBEDDER_FILENAME);

    if target.is_file() {
        eprintln!("  · verifying {EMBEDDER_FILENAME} ({EMBEDDER_MB} MB)…");
        if compute_sha256(&target).await? == EMBEDDER_SHA256 {
            return Ok((target, Staged::AlreadyPresent));
        }
        eprintln!("  ! checksum mismatch on the staged model — re-downloading");
        tokio::fs::remove_file(&target).await?;
    }

    tokio::fs::create_dir_all(&dir).await?;
    let partial = dir.join(format!("{EMBEDDER_FILENAME}.partial"));
    let guard = PartialGuard::new(partial.clone());

    eprintln!(
        "  · staging the embedder — {EMBEDDER_MB} MB, once per machine, into {}",
        dir.display()
    );
    download_with_progress(EMBEDDER_URL, &partial).await?;

    let got = compute_sha256(&partial).await?;
    anyhow::ensure!(
        got == EMBEDDER_SHA256,
        "checksum mismatch for {EMBEDDER_FILENAME}: expected {EMBEDDER_SHA256}, got {got}. \
         The partial download has been deleted. This means the mirror served different \
         bytes than we pinned — re-run to retry, and report it if it repeats."
    );

    guard.commit(&target).await?;
    Ok((target, Staged::Downloaded))
}

// ── Internals ────────────────────────────────────────────────────────────────

/// Deletes the `.partial` file unless [`Self::commit`] consumed it, so an
/// error, a panic or a Ctrl-C can never leave bytes that look staged.
struct PartialGuard {
    partial: PathBuf,
    committed: bool,
}

impl PartialGuard {
    fn new(partial: PathBuf) -> Self {
        Self { partial, committed: false }
    }

    async fn commit(mut self, target: &Path) -> std::io::Result<()> {
        tokio::fs::rename(&self.partial, target).await?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartialGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.partial);
        }
    }
}

async fn compute_sha256(path: &Path) -> anyhow::Result<String> {
    use tokio::io::AsyncReadExt as _;

    let mut file = tokio::fs::File::open(path).await?;
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
/// stdout carries the command's result and is what a script pipes; a progress
/// bar there would corrupt it.
async fn download_with_progress(url: &str, partial_path: &Path) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        // A stalled CDN is the most likely failure on a first run, and an
        // unbounded hang is indistinguishable from a slow link. Bound the
        // handshake; the body itself is deliberately unbounded because a slow
        // 253 MB download is legitimate.
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach the model mirror: {e}"))?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("model mirror refused the download: {e}"))?;

    let pb = match response.content_length() {
        // INVARIANT: stderr. stdout is the command's result.
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
    pb.set_message("granite-embedding-311m Q4_K_M");

    let mut file = tokio::fs::File::create(partial_path).await?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("download interrupted: {e}"))?;
        file.write_all(&chunk).await?;
        pb.inc(chunk.len() as u64);
    }
    file.flush().await?;
    pb.finish_with_message("embedder staged");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The staged filename is the ONE thing that has to agree with the engine.
    /// Staging 253 MB to a path `llamacpp_gguf_path` does not consult is a
    /// silent no-op that looks like success.
    #[test]
    fn the_staged_filename_is_the_name_the_engine_looks_for() {
        assert_eq!(EMBEDDER_FILENAME, "granite-embedding-311m-multilingual-r2.Q4_K_M.gguf");
    }

    #[test]
    fn the_pinned_digest_is_a_full_sha256() {
        assert_eq!(EMBEDDER_SHA256.len(), 64);
        assert!(EMBEDDER_SHA256.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// The bar must never draw on stdout — see [`download_with_progress`].
    #[test]
    fn progress_never_draws_on_stdout() {
        let src = include_str!("stage.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        assert!(production.contains("ProgressDrawTarget::stderr()"));
        assert!(
            !production.contains(concat!("ProgressDrawTarget", "::stdout()")),
            "stdout is the command's result; a progress bar there corrupts every pipe"
        );
    }

    #[tokio::test]
    async fn a_partial_download_never_survives_as_a_staged_model() {
        let td = tempfile::tempdir().unwrap();
        let partial = td.path().join("model.gguf.partial");
        tokio::fs::write(&partial, b"half a model").await.unwrap();

        drop(PartialGuard::new(partial.clone()));

        assert!(!partial.exists(), "an uncommitted partial must be deleted on drop");
    }

    #[tokio::test]
    async fn a_committed_partial_becomes_the_target() {
        let td = tempfile::tempdir().unwrap();
        let partial = td.path().join("model.gguf.partial");
        let target = td.path().join("model.gguf");
        tokio::fs::write(&partial, b"a whole model").await.unwrap();

        PartialGuard::new(partial.clone()).commit(&target).await.unwrap();

        assert!(!partial.exists());
        assert!(target.exists());
    }
}

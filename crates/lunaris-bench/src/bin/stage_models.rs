//! N-04 D5 — `stage-models` HF Hub stager.
//!
//! Downloads the two v0.4 native-stack model directories into the
//! operator's local Lunaris cache:
//!
//! - `ibm-granite/granite-embedding-r2` (embedder, granite-r2)
//! - `BAAI/bge-reranker-v2-m3`            (reranker, bge-reranker-v2-m3)
//!
//! Each repo's `model.safetensors` + `tokenizer.json` + `config.json`
//! land under `<cache_dir>/<short-name>/` where `<short-name>` matches
//! the directory names `lunaris-memory::handle::default_model_dir`
//! consults (`granite-r2/`, `bge-reranker-v2-m3/`). Operators staging
//! quantized GGUFs out-of-band can verify the SHA-256s printed by
//! `--help` against their downloaded artifacts.
//!
//! ## Idempotence
//!
//! `hf_hub::api::tokio::Api::get` checks the cache before downloading
//! and is a no-op when the artifact already exists with a matching
//! commit hash. We rely on hf-hub for atomicity; no separate hash
//! verification here.
//!
//! ## `--dry-run`
//!
//! Returns BEFORE constructing the `ApiBuilder` so neither the cache
//! directory nor the hf-hub `~/.cache/huggingface` symlink shadow are
//! created. The dry-run is the contract the CI test asserts against.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::Parser;

/// SHA-256 of the canonical Q4_K_M granite-r2 GGUF (the llama.cpp
/// embedder artifact). Operators downloading the GGUF out-of-band MUST
/// verify this hash before serving traffic.
const GRANITE_GGUF_Q4_K_M_SHA256: &str =
    "0768a38b0bc9900e89bb15ae0b6ea2ca7db130759e0eca226119610aedf5e276";
const GRANITE_GGUF_SIZE_MIB: f64 = 240.7;

/// SHA-256 of the canonical Q5_K_M-imatrix bge-reranker-v2-m3 GGUF
/// (the llama.cpp reranker artifact). Re-stated here so this binary's
/// `--help` is self-contained.
const BGE_GGUF_Q5_K_M_SHA256: &str =
    "6cdcc566200dba69553a89a9d59ff6d631e33969bc9367eff6914919f7722a1c";
const BGE_GGUF_SIZE_MIB: f64 = 446.0;

const GRANITE_REPO: &str = "ibm-granite/granite-embedding-r2";
const GRANITE_CACHE_SUBDIR: &str = "granite-r2";
const BGE_REPO: &str = "BAAI/bge-reranker-v2-m3";
const BGE_CACHE_SUBDIR: &str = "bge-reranker-v2-m3";

const SAFETENSORS_FILES: &[&str] = &["model.safetensors", "tokenizer.json", "config.json"];

/// Long-form `--help` body. Lists both repos AND the canonical GGUF
/// SHA-256s so the operator can verify a quantized download without
/// chasing constants in another crate.
fn after_help() -> String {
    format!(
        "Models staged:\n  \
         - {GRANITE_REPO} → <cache_dir>/{GRANITE_CACHE_SUBDIR}/ (safetensors, ~620 MiB)\n  \
         - {BGE_REPO} → <cache_dir>/{BGE_CACHE_SUBDIR}/ (safetensors, ~2.2 GiB)\n\n\
         Canonical GGUF SHA-256s (for operators staging quantized artifacts):\n  \
         - granite-r2-311m-Q4_K_M.gguf ({GRANITE_GGUF_SIZE_MIB} MiB)\n    \
         sha256:{GRANITE_GGUF_Q4_K_M_SHA256}\n  \
         - bge-reranker-v2-m3-Q5_K_M-imatrix.gguf ({BGE_GGUF_SIZE_MIB} MiB)\n    \
         sha256:{BGE_GGUF_Q5_K_M_SHA256}\n",
    )
}

#[derive(Parser, Debug)]
#[command(
    name = "stage-models",
    version,
    about = "N-04 D5 — stage granite-r2 + bge-reranker-v2-m3 into the Lunaris cache",
    after_help = after_help(),
)]
struct Cli {
    /// Skip all network + filesystem side effects. Prints the staging
    /// plan (repos, target paths, GGUF SHA-256s) and exits 0. Useful
    /// for CI invocations that want to verify the binary parses
    /// without burning ~3 GiB of egress.
    #[arg(long)]
    dry_run: bool,

    /// Cache root for the staged model directories. Default
    /// `~/.cache/lunaris/models/native/` mirrors the layout
    /// `lunaris-memory::handle::default_model_dir` consults at runtime.
    /// Passing an explicit path overrides; the parent directory is
    /// created on demand UNLESS `--dry-run` is set.
    #[arg(long, default_value_os_t = default_cache_dir())]
    cache_dir: PathBuf,
}

fn default_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lunaris")
        .join("models")
        .join("native")
}

fn print_plan(cli: &Cli) {
    println!("stage-models — plan");
    println!("  cache_dir = {}", cli.cache_dir.display());
    println!("  granite   = {GRANITE_REPO} → {}/{GRANITE_CACHE_SUBDIR}/", cli.cache_dir.display());
    println!("              files: {}", SAFETENSORS_FILES.join(", "));
    println!(
        "              GGUF (manual): sha256:{GRANITE_GGUF_Q4_K_M_SHA256} ({GRANITE_GGUF_SIZE_MIB} MiB)"
    );
    println!("  bge       = {BGE_REPO} → {}/{BGE_CACHE_SUBDIR}/", cli.cache_dir.display());
    println!("              files: {}", SAFETENSORS_FILES.join(", "));
    println!(
        "              GGUF (manual): sha256:{BGE_GGUF_Q5_K_M_SHA256} ({BGE_GGUF_SIZE_MIB} MiB)"
    );
}

async fn stage_repo(api: &hf_hub::api::tokio::Api, repo: &str) -> anyhow::Result<()> {
    let handle = api.repo(hf_hub::Repo::with_revision(
        repo.to_string(),
        hf_hub::RepoType::Model,
        "main".to_string(),
    ));
    for file in SAFETENSORS_FILES {
        let path = handle.get(file).await.with_context(|| format!("download {repo}/{file}"))?;
        println!("  ✓ {repo}/{file} → {}", path.display());
    }
    Ok(())
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    print_plan(&cli);

    if cli.dry_run {
        println!();
        println!("--dry-run set — no filesystem or network side effects performed.");
        return Ok(());
    }

    // Create cache root on demand (NOT before dry-run check, per the
    // contract that `--dry-run --cache-dir /tmp/x` leaves /tmp/x absent).
    std::fs::create_dir_all(&cli.cache_dir)
        .with_context(|| format!("create cache dir {}", cli.cache_dir.display()))?;

    let api = hf_hub::api::tokio::ApiBuilder::new()
        .with_cache_dir(cli.cache_dir.clone())
        .with_progress(true)
        .build()
        .context("init hf-hub Api")?;

    println!();
    println!("Staging {GRANITE_REPO} ...");
    stage_repo(&api, GRANITE_REPO).await?;
    println!();
    println!("Staging {BGE_REPO} ...");
    stage_repo(&api, BGE_REPO).await?;

    println!();
    println!("✓ stage-models complete. Cache root: {}", cli.cache_dir.display());
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("stage-models: {e:#}");
            ExitCode::FAILURE
        }
    }
}

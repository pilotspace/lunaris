//! Direct-open fallback: run the request in-process when contextd is not there.
//!
//! This exists so the CLI still works on a machine with no daemon (a CI job, a
//! one-off inspection, a fresh install). It runs the **identical**
//! [`lunaris_memory_service::protocol::dispatch`] the daemon runs — that
//! sameness is the property worth protecting, and the reason the fallback is a
//! few lines rather than a parallel implementation.
//!
//! Storage resolution deliberately reuses `lunaris_core::store_discovery`
//! rather than re-deriving a URL: `LUNARIS_STORE_URL`, else the store a live
//! contextd advertises in `~/.lunaris/contextd-moon.url` (adopted only after
//! the shared liveness probe answers). There is no default. Guessing
//! `moon://127.0.0.1:6379` would silently point at whatever Redis-compatible
//! server happens to be listening — on this project's own dev box, 6380 is an
//! unrelated ai-proxy Redis, and a TCP probe against it passes.

use std::sync::Arc;

use anyhow::Context as _;
use lunaris::Lunaris;
use lunaris_core::Scope;
use lunaris_memory_service::protocol::MemoryRequest;
use serde_json::Value;

/// Help text for the no-store case. Names both fixes, because the two failure
/// modes need different actions and an operator staring at "needs a storage
/// URL" cannot tell them apart.
const NO_STORE_HELP: &str = "\
no Lunaris store could be resolved.

Either:
  * start the daemon         — `lunaris-contextd` publishes its store in
                               ~/.lunaris/contextd-moon.url, or
  * point at a store directly — LUNARIS_STORE_URL=moon://127.0.0.1:<port>

There is deliberately no default: a guessed port can land on an unrelated
Redis-compatible server, which answers a TCP probe and then behaves nothing
like a Moon.";

/// Resolve the store URL the same way every other Lunaris surface does.
pub(crate) fn resolve_store_url() -> anyhow::Result<String> {
    // An empty LUNARIS_STORE_URL falls through to discovery rather than
    // failing: `export LUNARIS_STORE_URL=` reads as "unset it", and treating it
    // as a store URL would produce a confusing open error instead.
    if let Ok(url) = std::env::var("LUNARIS_STORE_URL")
        && !url.trim().is_empty()
    {
        return Ok(url);
    }
    let home = dirs_home().context("cannot determine $HOME to look for ~/.lunaris")?;
    lunaris_core::store_discovery::discover_contextd_moon(&home.join(".lunaris"))
        .into_url()
        .ok_or_else(|| anyhow::anyhow!(NO_STORE_HELP))
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Open a handle and run the request through the shared dispatch.
pub(crate) async fn dispatch_direct(req: MemoryRequest) -> anyhow::Result<Value> {
    let scope =
        Scope::new(req.scope()).with_context(|| format!("invalid scope {:?}", req.scope()))?;
    let url = resolve_store_url()?;
    let lunaris = open_handle(&url, None).await?;
    dispatch_on(&lunaris, &scope, req).await
}

/// Open a handle at an EXPLICIT url, optionally with a caller-supplied embedder.
///
/// This is the crate's single storage-open site — `tests/single_storage_open.rs`
/// enforces that, and the reason is the three pre-GA-1 recall pipelines that
/// drifted apart precisely because each surface held its own handle. `lunaris
/// try` needs a handle too (it drives seven dispatches against an embedded Moon
/// it just started), so it comes HERE for it rather than growing a second open.
///
/// `url` is a parameter rather than a resolution, because `try`'s whole safety
/// story is that its URL comes from a launcher that bound `127.0.0.1:0` — never
/// from the environment, never from contextd discovery. Passing the URL in is
/// what makes "cannot reach a real store" a property of the call site instead of
/// a promise in a comment.
///
/// `embedder` is `None` on every production path, which means the engine
/// resolves llama.cpp / remote / Noop exactly as `Lunaris::open` does. It is
/// `Some` only for the documented `LUNARIS_TRY_EMBEDDER=stub` plumbing seam.
pub(crate) async fn open_handle(
    url: &str,
    embedder: Option<Arc<dyn lunaris_core::Embedder>>,
) -> anyhow::Result<Arc<Lunaris>> {
    let engine = match embedder {
        Some(e) => Lunaris::open_with_embedder(url, e)
            .await
            .with_context(|| format!("open {url} with a caller-supplied embedder"))?,
        None => Lunaris::open(url).await.with_context(|| format!("open {url}"))?,
    };
    Ok(Arc::new(engine))
}

/// Run one request through the shared dispatch on an already-open handle.
///
/// Separate from [`open_handle`] so a caller with many requests — `lunaris try`
/// ingests six samples before it recalls — pays the model load once. Both the
/// one-shot and the many-shot path end at the identical `dispatch`.
pub(crate) async fn dispatch_on(
    lunaris: &Arc<Lunaris>,
    scope: &Scope,
    req: MemoryRequest,
) -> anyhow::Result<Value> {
    lunaris_memory_service::protocol::dispatch(lunaris, scope, req)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}

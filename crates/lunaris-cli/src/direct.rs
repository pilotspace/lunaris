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
    let lunaris = Arc::new(Lunaris::open(&url).await.with_context(|| format!("open {url}"))?);
    lunaris_memory_service::protocol::dispatch(&lunaris, &scope, req)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}

//! `lunaris-hook` library — hook envelope processing.
//!
//! `main.rs` is thin: it reads stdin, calls `run`, and maps the result to
//! a sysexits.h exit code. All testable logic lives here.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

pub mod envelope;
pub mod filter;
pub mod ingest;
pub mod scope;

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::{Lsn, Scope};

/// Errors returned by [`run`].
///
/// Note: unknown event kind is NOT an error — `run` returns `Ok(None)` for
/// unknown kinds (caller exits 0). Only actual failures produce `Err`.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    /// JSON parse failure or missing required field (exit 64).
    #[error("envelope parse error: {0}")]
    Parse(#[from] envelope::ParseError),

    /// Storage substrate rejected the write (exit 65).
    #[error("ingest error: {0}")]
    Ingest(#[from] lunaris_core::LunarisError),

    /// Event rejected by filter policy — exit 66 (no Episode written, not an error).
    #[error("event filtered by policy: {0}")]
    Filtered(String),
}

impl HookError {
    /// Map to a sysexits.h-style exit code.
    pub fn exit_code(&self) -> i32 {
        match self {
            HookError::Parse(_) => 64,
            HookError::Ingest(_) => 65,
            HookError::Filtered(_) => 66,
        }
    }
}

/// Process one hook envelope.
///
/// Returns:
/// - `Ok(Some(lsn))` — Episode written, caller exits 0.
/// - `Ok(None)` — Unknown event kind, no Episode written, caller exits 0.
/// - `Err(HookError::Parse)` — Malformed JSON or missing field, caller exits 64.
/// - `Err(HookError::Ingest)` — Storage rejected the write, caller exits 65.
///
/// Exit 66 is reserved for Phase 24 filter-rejected events (not used here).
pub async fn run(
    stdin_bytes: &[u8],
    scope: Scope,
    lunaris: Arc<Lunaris>,
) -> Result<Option<Lsn>, HookError> {
    let event = envelope::parse(stdin_bytes)?;

    match &event {
        envelope::HookEvent::Unknown(kind) => {
            tracing::info!(kind = %kind, "unknown hook event kind — no-op (exit 0)");
            return Ok(None);
        }
        _ => {}
    }

    let builder = ingest::build_episode(&event)
        .expect("non-Unknown event must produce an EpisodeBuilder");

    let scoped = lunaris.scoped(scope);
    let lsn = scoped.ingest(builder).await.map_err(HookError::Ingest)?;
    Ok(Some(lsn))
}

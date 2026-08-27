//! W4.6 / D6.4 — enforcing a scope's [`RetentionPolicy`].
//!
//! The policy shape and its KV location live in [`lunaris_core::retention`];
//! this is the sweeper.
//!
//! ## It goes through `forget`, not around it
//!
//! The D6 decision named the interaction: `forget` soft-deletes by default,
//! and retention that hard-deletes must not silently change what `.hard()`
//! means. So a sweep is an ordinary scoped `ForgetTarget::Before` — it gets
//! the same chunk sweep, the same single-`atomic_write` invariant, the same
//! soft/hard semantics and the same audit receipt a human `forget` gets. A
//! sweeper that reached past `forget` into `atomic_write` would be a second,
//! quieter definition of delete, and the two would drift.
//!
//! Hard sweeps obtain a confirmation token the way a human does: run the
//! preview, derive the token from THAT receipt, apply. The D-21 rail keeps
//! meaning what it means — the policy is the standing authorization, not a
//! bypass of the check.
//!
//! ## No scheduler here
//!
//! `enforce` is a function an operator or a job calls. Lunaris does not spawn
//! a retention daemon: a background thread that deletes data on a timer, in a
//! library the host embeds, is not a decision this crate gets to make for its
//! host. The MCP / hook / HTTP surfaces can expose it; the scheduling belongs
//! to whoever owns the deployment.

#![forbid(unsafe_code)]

use lunaris_core::retention::{RetentionPolicy, retention_policy_key};
use lunaris_core::{Hlc, LunarisError, Scope, StoragePort, WriteOp};

use crate::forget::{ForgetReceipt, ForgetTarget};

/// What one [`enforce_at`] pass did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RetentionReceipt {
    /// `None` when the scope has no policy — nothing was swept and nothing
    /// was audited.
    pub policy: Option<RetentionPolicy>,
    /// The valid-time cutoff the sweep used, when it ran.
    pub cutoff: Option<Hlc>,
    /// The underlying forget receipt, when the sweep ran.
    pub forget: Option<ForgetReceipt>,
}

impl RetentionReceipt {
    /// Rows this pass soft-stamped or hard-deleted. `0` when no policy is set.
    pub fn rows_swept(&self) -> u64 {
        match &self.forget {
            Some(r) => r.rows_written + r.rows_deleted,
            None => 0,
        }
    }
}

/// Read `scope`'s policy, or `None` when it has none.
pub async fn read_policy(
    storage: &std::sync::Arc<dyn StoragePort>,
    scope: &Scope,
    clock: &lunaris_core::HlcClock,
) -> Result<Option<RetentionPolicy>, LunarisError> {
    let key = retention_policy_key(scope);
    let row = storage.read_as_of(scope, &key, clock.tick()).await.map_err(LunarisError::Storage)?;
    let Some(row) = row else { return Ok(None) };
    match serde_json::from_slice::<RetentionPolicy>(&row.value) {
        Ok(p) => Ok(Some(p)),
        // A policy that does not parse must NOT read as "keep everything" —
        // that would silently disable retention for the scope. Surface it.
        Err(e) => Err(LunarisError::Storage(lunaris_core::error::StorageError::Backend(format!(
            "retention policy for scope `{}` did not parse: {e}",
            scope.as_str()
        )))),
    }
}

/// Write `scope`'s policy.
pub async fn write_policy(
    storage: &std::sync::Arc<dyn StoragePort>,
    scope: &Scope,
    policy: RetentionPolicy,
) -> Result<(), LunarisError> {
    let value = serde_json::to_vec(&policy).map_err(|e| {
        LunarisError::Storage(lunaris_core::error::StorageError::Backend(format!(
            "retention policy serialize: {e}"
        )))
    })?;
    storage
        .atomic_write(scope, &[WriteOp::KvPut { key: retention_policy_key(scope), value }])
        .await
        .map_err(LunarisError::Storage)?;
    Ok(())
}

/// Run one retention pass over `scope` against the wall-clock `now_ms`.
///
/// `now_ms` is a parameter rather than read from the clock inside so a caller
/// can pin the cutoff — for a backfill, for a replay, and so a test asserts on
/// a cutoff it chose rather than on one that moved while it ran.
pub async fn enforce_at(
    engine: &crate::handle::Lunaris,
    scope: &Scope,
    now_ms: u64,
) -> Result<RetentionReceipt, LunarisError> {
    run(engine, scope, now_ms, Mode::Commit).await
}

/// Report what [`enforce_at`] would take, taking nothing.
///
/// Wave 6 / R1. The MCP surface that exposes retention is LLM-driven, and the
/// `memory.forget` ruling (0.6.2 Task F) is that such a surface previews by
/// default. Without this, a caller wanting a dry run had to recompute
/// `now - max_age_ms` itself — a second definition of the cutoff, and this
/// module exists because two definitions of a delete drift apart. The preview
/// therefore shares one cutoff computation with the commit, in `run`.
///
/// A preview of a `hard` policy is still a preview: it takes the `dry_run`
/// branch and never mints a confirmation token, so previewing cannot be a
/// path to a hard delete.
pub async fn preview_at(
    engine: &crate::handle::Lunaris,
    scope: &Scope,
    now_ms: u64,
) -> Result<RetentionReceipt, LunarisError> {
    run(engine, scope, now_ms, Mode::Preview).await
}

/// Whether a [`run`] pass commits or only reports.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Commit,
    Preview,
}

/// The one place a retention cutoff is computed. Both the commit and the
/// preview enter here so they cannot disagree about which rows are eligible.
async fn run(
    engine: &crate::handle::Lunaris,
    scope: &Scope,
    now_ms: u64,
    mode: Mode,
) -> Result<RetentionReceipt, LunarisError> {
    let storage = engine.storage();
    let Some(policy) = read_policy(&storage, scope, &engine.clock()).await? else {
        return Ok(RetentionReceipt { policy: None, cutoff: None, forget: None });
    };

    // Saturating: a `max_age_ms` larger than the current wall clock means the
    // cutoff is the epoch, which is "nothing is old enough yet" — the correct
    // reading of a very long retention window, and not an underflow panic.
    let cutoff = Hlc { wall_ms: now_ms.saturating_sub(policy.max_age_ms), counter: 0, node_id: 0 };
    let scoped = engine.scoped(scope.clone());

    let forget = if mode == Mode::Preview {
        // A preview never mints a confirmation token, whatever the policy
        // says: `hard` authorizes the sweep, not a shortcut around D-21.
        scoped.forget(ForgetTarget::Before(cutoff).dry_run()).await?
    } else if policy.hard {
        // Take the preview first and derive the token from it, exactly as a
        // human hard-forget must. The policy authorizes the sweep; it does not
        // excuse it from the D-21 rail.
        let preview = scoped.forget(ForgetTarget::Before(cutoff).dry_run()).await?;
        let token = engine.confirm_hard_forget(preview).await?;
        scoped.forget(ForgetTarget::Before(cutoff).hard().with_token(token)).await?
    } else {
        scoped.forget(ForgetTarget::Before(cutoff)).await?
    };

    Ok(RetentionReceipt { policy: Some(policy), cutoff: Some(cutoff), forget: Some(forget) })
}

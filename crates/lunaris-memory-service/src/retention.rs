//! `memory.retention` / `memory.retention_enforce` — the operator surface for
//! per-scope retention.
//!
//! ## Why this exists
//!
//! `ScopedLunaris::{retention_policy, set_retention_policy, enforce_retention}`
//! shipped in W4.6 and, until Wave 6, had **zero callers outside its own test
//! file**. That was half a decision. The engine deliberately ships no
//! scheduler — `crates/lunaris/src/retention.rs` explains why a library the
//! host embeds does not get to start a thread that deletes data on a timer,
//! and names the intended resolution: "The MCP / hook / HTTP surfaces can
//! expose it; the scheduling belongs to whoever owns the deployment."
//!
//! None of them did. So the escape hatch existed only for a caller writing
//! Rust against the engine directly, which is nobody using Lunaris through
//! MCP — and nothing at all cleaned up memories on any surface an agent or an
//! operator could reach. This is that surface.
//!
//! ## Two ops, not three
//!
//! The engine has three methods; the wire has two tools, because "read the
//! policy" and "write the policy" are the same question asked with and
//! without an argument, and an LLM picking between `get_x` and `set_x` picks
//! wrong more often than it omits a field.
//!
//! - `memory.retention` — omit `max_age_ms` to read; supply it to set.
//! - `memory.retention_enforce` — run a pass. **`dry_run` defaults to `true`.**
//!
//! ## `dry_run` defaults to TRUE, matching `memory.forget`
//!
//! Same ruling, same reason (0.6.2 Task F): this surface is driven by an LLM,
//! so an irreversible sweep is not the default. The preview goes through
//! `ScopedLunaris::preview_retention`, which shares one cutoff computation
//! with the commit — the service layer does NOT recompute `now - max_age_ms`,
//! because two definitions of which rows are eligible would drift exactly the
//! way the engine module refuses to let soft and hard delete drift.
//!
//! ## `hard` is sticky, and that is deliberate
//!
//! `hard` is a property of the stored policy, not of a call. Setting a policy
//! without `hard` writes `hard: false` — the recoverable default the engine
//! documents ("the failure mode of an accidental policy is unrecoverable data
//! loss and the failure mode of an accidentally-absent one is disk"). An
//! agent that wants a hard policy must say so on the `memory.retention` call
//! that writes it; it cannot escalate at enforce time.
//!
//! The `scope` argument is the only partition key; neither wire DTO carries a
//! `scope` or `tenant` field (CLAUDE.md DTO discipline).

use serde::{Deserialize, Serialize};

use crate::ServiceError;
use lunaris::Lunaris;
use lunaris_core::Scope;
use lunaris_core::retention::RetentionPolicy;

// ── memory.retention ──────────────────────────────────────────────────────────

/// Input parameters for `memory.retention`.
#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetentionParams {
    /// Maximum age, in milliseconds, that a memory may reach before a sweep
    /// is allowed to take it.
    ///
    /// **Omit to READ the current policy without changing it.** Supplying it
    /// writes the policy, replacing any previous one.
    #[serde(default)]
    pub max_age_ms: Option<u64>,

    /// Whether a sweep under this policy hard-deletes (unrecoverable) rather
    /// than soft-deleting (hidden from recall, recoverable).
    ///
    /// Only meaningful alongside `max_age_ms`; ignored on a read. Defaults to
    /// `false` — see the module docs on why the recoverable mode is the
    /// default even when the caller says nothing.
    #[serde(default)]
    pub hard: Option<bool>,
}

/// Output of `memory.retention`.
///
/// Flat struct by contract: rmcp 1.7 aborts server startup when a tool's
/// generated `outputSchema` root is not `type: "object"`, so the read/write
/// outcome is a `status` **field**, never a `#[serde(tag = …)]` enum
/// discriminator (CLAUDE.md MCP tool-schema invariant; guard:
/// `crates/lunaris-mcp/tests/server_boot.rs`).
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RetentionResponse {
    /// `"read"` when the call only reported, `"set"` when it wrote a policy.
    pub status: String,

    /// `false` when this scope has no retention policy at all — in which case
    /// nothing will ever be swept from it, by anything.
    pub configured: bool,

    /// The policy's maximum age, or `null` when unconfigured.
    pub max_age_ms: Option<u64>,

    /// Whether a sweep under this policy hard-deletes. `false` when
    /// unconfigured (there is no sweep to characterise).
    pub hard: bool,
}

/// Execute `memory.retention`: read the policy, or write it when
/// `max_age_ms` is supplied.
///
/// Reading a scope whose stored policy does not parse is an ERROR, not an
/// empty read — the engine's `read_policy` surfaces it deliberately, because
/// reporting a corrupt policy as "no policy" silently disables retention for
/// the scope.
pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    params: RetentionParams,
) -> Result<RetentionResponse, ServiceError> {
    let scoped = lunaris.scoped(scope.clone());

    let (status, policy) = match params.max_age_ms {
        Some(max_age_ms) => {
            let mut policy = RetentionPolicy::max_age_ms(max_age_ms);
            if params.hard.unwrap_or(false) {
                policy = policy.hard();
            }
            scoped.set_retention_policy(policy).await?;
            ("set", Some(policy))
        }
        None => ("read", scoped.retention_policy().await?),
    };

    tracing::debug!(
        scope = scope.as_str(),
        status = status,
        configured = policy.is_some(),
        "memory.retention completed",
    );

    Ok(RetentionResponse {
        status: status.to_string(),
        configured: policy.is_some(),
        max_age_ms: policy.map(|p| p.max_age_ms),
        hard: policy.is_some_and(|p| p.hard),
    })
}

// ── memory.retention_enforce ──────────────────────────────────────────────────

/// `dry_run` default for the MCP surface: **preview**. See the module docs.
fn default_dry_run() -> bool {
    true
}

/// Input parameters for `memory.retention_enforce`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetentionEnforceParams {
    /// Preview switch — **defaults to `true`**.
    ///
    /// `true` (or omitted): report what a sweep would take and take nothing.
    /// `false`: run the sweep, under whatever mode the stored policy sets.
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

/// Output of `memory.retention_enforce`.
///
/// Flat by the same rmcp contract as [`RetentionResponse`].
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RetentionEnforceResponse {
    /// `"no_policy"` when the scope has no policy (nothing ran),
    /// `"preview"` when nothing was taken, `"swept"` when the sweep committed.
    pub status: String,

    /// The effective preview flag, echoed from the engine receipt rather than
    /// from the request, so it cannot disagree with what the engine did.
    pub dry_run: bool,

    /// `false` when the scope has no policy — the case an operator most needs
    /// to be told apart from "a sweep ran and found nothing".
    pub configured: bool,

    /// The policy's maximum age, or `null` when unconfigured.
    pub max_age_ms: Option<u64>,

    /// Whether the policy hard-deletes.
    pub hard: bool,

    /// The wall-clock cutoff the pass used, in milliseconds: memories whose
    /// valid time starts before this are eligible. `null` when nothing ran.
    pub cutoff_ms: Option<u64>,

    /// Episodes the cutoff matched — what a committing pass WOULD take.
    /// Reported on both paths, `0` when no policy is set.
    pub matched: u64,

    /// Episodes actually taken. Always `0` on a preview or with no policy.
    pub removed: u64,
}

/// Execute `memory.retention_enforce`.
///
/// | `dry_run` | policy | Engine call                             | `status`     |
/// |-----------|--------|-----------------------------------------|--------------|
/// | any       | none   | either (both no-op)                     | `no_policy`  |
/// | `true`    | set    | `ScopedLunaris::preview_retention`      | `preview`    |
/// | `false`   | set    | `ScopedLunaris::enforce_retention`      | `swept`      |
///
/// A hard policy previewed is still a preview: the engine's preview branch
/// never mints a D-21 confirmation token, so no route through this tool can
/// hard-delete without `dry_run: false` AND a policy that already said `hard`.
pub async fn handle_enforce(
    lunaris: &Lunaris,
    scope: &Scope,
    params: RetentionEnforceParams,
) -> Result<RetentionEnforceResponse, ServiceError> {
    let scoped = lunaris.scoped(scope.clone());

    let receipt = if params.dry_run {
        scoped.preview_retention().await?
    } else {
        scoped.enforce_retention().await?
    };

    // Read the outcome off the receipt, never off the request: `preview` is
    // what the engine actually did.
    let (status, dry_run, matched, removed) = match receipt.forget.as_ref() {
        None => ("no_policy", params.dry_run, 0, 0),
        Some(f) if f.preview => ("preview", true, f.matched, 0),
        Some(f) => ("swept", false, f.matched, f.matched),
    };

    tracing::debug!(
        scope = scope.as_str(),
        status = status,
        matched = matched,
        removed = removed,
        "memory.retention_enforce completed",
    );

    Ok(RetentionEnforceResponse {
        status: status.to_string(),
        dry_run,
        configured: receipt.policy.is_some(),
        max_age_ms: receipt.policy.map(|p| p.max_age_ms),
        hard: receipt.policy.is_some_and(|p| p.hard),
        cutoff_ms: receipt.cutoff.map(|c| c.wall_ms),
        matched,
        removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The MCP ruling, pinned at the type level rather than in prose: omitting
    /// `dry_run` must deserialize to a PREVIEW. A default that flipped to
    /// `false` in a refactor would turn every unqualified enforce call into an
    /// irreversible sweep, and nothing else in the crate would notice.
    #[test]
    fn omitted_dry_run_previews() {
        let p: RetentionEnforceParams = serde_json::from_str("{}").expect("empty params");
        assert!(p.dry_run, "memory.retention_enforce defaulted to COMMIT, not preview");
    }

    /// An explicit `false` must still reach the commit path — a default that
    /// could not be overridden would be a different bug with the same shape.
    #[test]
    fn explicit_dry_run_false_commits() {
        let p: RetentionEnforceParams =
            serde_json::from_str(r#"{"dry_run":false}"#).expect("params");
        assert!(!p.dry_run);
    }

    /// DTO discipline: a client must not be able to smuggle a `scope` past the
    /// socket-bound partition key.
    #[test]
    fn both_dtos_reject_unknown_fields() {
        assert!(
            serde_json::from_str::<RetentionParams>(r#"{"scope":"other"}"#).is_err(),
            "RetentionParams accepted a wire-side scope"
        );
        assert!(
            serde_json::from_str::<RetentionEnforceParams>(r#"{"tenant":"other"}"#).is_err(),
            "RetentionEnforceParams accepted a wire-side tenant"
        );
    }

    /// Reading is the no-argument call. If `max_age_ms` ever stopped being
    /// optional, every read would become a write of whatever the caller
    /// guessed.
    #[test]
    fn omitted_max_age_reads() {
        let p: RetentionParams = serde_json::from_str("{}").expect("empty params");
        assert!(p.max_age_ms.is_none(), "an empty memory.retention call would WRITE a policy");
        assert!(p.hard.is_none());
    }

    /// `hard` must not be settable to `true` by omission. The engine's own
    /// comment says the recoverable failure mode is the one to default to.
    #[test]
    fn omitted_hard_is_not_hard() {
        let p: RetentionParams = serde_json::from_str(r#"{"max_age_ms":1000}"#).expect("params");
        assert!(!p.hard.unwrap_or(false), "an unqualified policy defaulted to HARD");
    }
}

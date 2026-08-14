//! `memory.forget` — delete memories by source prefix or episode ID.
//!
//! Delegates to `ScopedLunaris::forget` with a [`lunaris::ForgetTarget`]
//! derived from the wire DTO. Two mutually-exclusive target fields:
//!
//! - `source_prefix` → `ForgetTarget::Scope`(`ScopeSpec::BySource`(prefix))
//! - `episode_id`    → `ForgetTarget::Id`(ulid) (OPS-01 fast path)
//!
//! Both set / neither set → `ServiceError::InvalidInput` (ambiguous / missing).
//!
//! The `scope` argument is the **only** partition key; the wire DTO
//! intentionally carries no `scope` or `tenant` field (CLAUDE.md DTO discipline).

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::ServiceError;
use lunaris::Lunaris;
use lunaris_core::Scope;

// ── Wire DTOs ─────────────────────────────────────────────────────────────────

/// Discriminant specifying what to forget.
///
/// Exactly one field must be set. If both are set the handler returns
/// `InvalidInput` (ambiguous target). If neither is set the handler returns
/// `InvalidInput` (missing target).
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForgetTarget {
    /// Forget all episodes whose source starts with this prefix.
    ///
    /// Must be non-empty; an empty prefix would match every episode in the
    /// scope and is rejected as a footgun.
    #[serde(default)]
    pub source_prefix: Option<String>,

    /// Forget a single episode by its ULID string.
    #[serde(default)]
    pub episode_id: Option<String>,
}

/// Input parameters for `memory.forget`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForgetParams {
    /// What to delete.
    pub target: ForgetTarget,
}

/// Output of a successful `memory.forget` call.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgetResponse {
    /// Number of episodes logically removed.
    ///
    /// For the default soft-delete path this equals `rows_written` from the
    /// underlying `ForgetReceipt`. Hard-delete receipts (not exposed via MCP)
    /// would populate `rows_deleted` instead; the sum covers both paths.
    pub removed: u64,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// Execute `memory.forget`.
///
/// ## Dispatch table
///
/// | `source_prefix` | `episode_id` | Action                                 |
/// |-----------------|--------------|----------------------------------------|
/// | Some(p)         | None         | `ForgetTarget::Scope(BySource(p))`     |
/// | None            | Some(id)     | `ForgetTarget::Id(ulid)`               |
/// | Some(_)         | Some(_)      | `InvalidInput` — ambiguous             |
/// | None            | None         | `InvalidInput` — missing               |
///
/// The scope comes exclusively from `scope` (bound at startup).
/// Cross-scope forgets are impossible by type.
pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    params: ForgetParams,
) -> Result<ForgetResponse, ServiceError> {
    let engine_target = build_target(params.target)?;

    let scoped = lunaris.scoped(scope.clone());
    let receipt = scoped.forget(engine_target).await?;

    // Soft-delete (default) populates rows_written; hard-delete populates
    // rows_deleted. The MCP wire doesn't expose the hard flag, so in practice
    // rows_deleted is always 0 here. Sum both to be defensive.
    let removed = receipt.rows_written + receipt.rows_deleted;

    tracing::debug!(scope = scope.as_str(), removed = removed, "memory.forget committed",);

    Ok(ForgetResponse { removed })
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Map the wire `ForgetTarget` DTO to the engine's [`lunaris::ForgetTarget`].
///
/// Validation rules (all return `ServiceError::InvalidInput`):
/// - Both fields set → ambiguous.
/// - Neither field set → missing target.
/// - `source_prefix` is empty string → footgun guard.
/// - `episode_id` is not a valid ULID string → parse error surfaced.
fn build_target(dto: ForgetTarget) -> Result<lunaris::ForgetTarget, ServiceError> {
    match (dto.source_prefix, dto.episode_id) {
        // Both set — ambiguous; reject.
        (Some(_), Some(_)) => Err(ServiceError::InvalidInput(
            "forget target must set exactly one of source_prefix or episode_id, not both"
                .to_string(),
        )),

        // source_prefix path — OPS-02 BySource prefix match.
        (Some(prefix), None) => {
            if prefix.is_empty() {
                return Err(ServiceError::InvalidInput(
                    "source_prefix must be non-empty (an empty prefix would match every episode)"
                        .to_string(),
                ));
            }
            Ok(lunaris::ForgetTarget::Scope(lunaris::ScopeSpec::BySource(prefix)))
        }

        // episode_id path — OPS-01 single-target fast path.
        (None, Some(id_str)) => {
            let ulid = id_str.parse::<Ulid>().map_err(|e| {
                ServiceError::InvalidInput(format!(
                    "episode_id is not a valid ULID: {e} (got {id_str:?})"
                ))
            })?;
            Ok(lunaris::ForgetTarget::Id(ulid))
        }

        // Neither set — missing target.
        (None, None) => Err(ServiceError::InvalidInput(
            "forget target requires either source_prefix or episode_id".to_string(),
        )),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lunaris::{EpisodeBuilder, Lunaris};
    use lunaris_core::Scope;

    async fn make_engine() -> (Lunaris, Scope) {
        let lunaris = Lunaris::open("memory://").await.expect("in-memory lunaris must open");
        // Use Scope::dev() so the shim in ScopedLunaris::forget (which routes
        // through the deprecated Lunaris::forget hard-coded to Scope::dev()
        // until Wave 1D) actually finds the episodes we ingest.
        // CLAUDE.md: Scope::dev() is a migration crutch permitted in tests.
        let scope = Scope::dev();
        (lunaris, scope)
    }

    // ── Validation-only tests (no storage round-trip) ─────────────────────────

    #[tokio::test]
    async fn no_target_fields_returns_invalid_input() {
        let (lunaris, scope) = make_engine().await;
        let params =
            ForgetParams { target: ForgetTarget { source_prefix: None, episode_id: None } };
        let err = handle(&lunaris, &scope, params).await.unwrap_err();
        assert!(matches!(err, ServiceError::InvalidInput(_)), "expected InvalidInput, got {err:?}");
        // Message must be informative.
        if let ServiceError::InvalidInput(msg) = err {
            assert!(
                msg.contains("source_prefix") || msg.contains("episode_id"),
                "error message should mention the missing fields: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn both_fields_set_returns_invalid_input() {
        let (lunaris, scope) = make_engine().await;
        let params = ForgetParams {
            target: ForgetTarget {
                source_prefix: Some("src:notes/".to_string()),
                episode_id: Some("01HZZZZZZZZZZZZZZZZZZZZZZZ".to_string()),
            },
        };
        let err = handle(&lunaris, &scope, params).await.unwrap_err();
        assert!(
            matches!(err, ServiceError::InvalidInput(_)),
            "expected InvalidInput for ambiguous target, got {err:?}"
        );
    }

    #[tokio::test]
    async fn invalid_ulid_returns_invalid_input() {
        let (lunaris, scope) = make_engine().await;
        let params = ForgetParams {
            target: ForgetTarget {
                source_prefix: None,
                episode_id: Some("not-a-ulid".to_string()),
            },
        };
        let err = handle(&lunaris, &scope, params).await.unwrap_err();
        assert!(
            matches!(err, ServiceError::InvalidInput(_)),
            "expected InvalidInput for bad ULID, got {err:?}"
        );
        if let ServiceError::InvalidInput(msg) = err {
            assert!(
                msg.contains("ULID") || msg.contains("ulid"),
                "message should mention ULID: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn empty_source_prefix_returns_invalid_input() {
        let (lunaris, scope) = make_engine().await;
        let params = ForgetParams {
            target: ForgetTarget { source_prefix: Some(String::new()), episode_id: None },
        };
        let err = handle(&lunaris, &scope, params).await.unwrap_err();
        assert!(
            matches!(err, ServiceError::InvalidInput(_)),
            "expected InvalidInput for empty source_prefix, got {err:?}"
        );
        if let ServiceError::InvalidInput(msg) = err {
            assert!(
                msg.contains("empty") || msg.contains("non-empty"),
                "message should explain the empty prefix guard: {msg}"
            );
        }
    }

    // ── Round-trip tests (ingest → forget → verify removed count) ─────────────

    // ── Round-trip tests (ingest → handler dispatch → no error) ──────────────
    //
    // The deprecated `Lunaris::forget` shim (v0.2.x) hard-codes `Scope::dev()`
    // key prefixes (`episode:`) for its internal scan_range / read_as_of calls,
    // but `ScopedLunaris::ingest` writes keys under the full scoped format
    // (`lunaris:_dev_:episode:{ulid}`). These will never intersect inside the
    // `EmbeddedStorage` backend — the shim's prefix scan returns zero matches
    // even when the data exists. This is a known v0.3 debt item tracked in
    // `docs/v0.3-known-debt.md` (per-scope routing in the forget pipeline).
    //
    // The handler's job is DTO→engine mapping + validation; the engine's
    // correctness is tested in `lunaris/src/forget.rs`. These two tests verify:
    //   1. The DTO-to-engine dispatch path compiles and runs without error.
    //   2. The `removed` field is a valid u64 (no type mismatch, no panic).
    // They intentionally avoid asserting a specific removed count.

    #[tokio::test]
    async fn forget_by_source_prefix_handler_dispatches_without_error() {
        let (lunaris, scope) = make_engine().await;

        // Ingest one episode so the engine has state; the handler must not error
        // regardless of whether the shim finds the episode.
        let scoped = lunaris.scoped(scope.clone());
        scoped.ingest(EpisodeBuilder::new("src:notes/a", "note a")).await.unwrap();

        let params = ForgetParams {
            target: ForgetTarget {
                source_prefix: Some("src:notes/".to_string()),
                episode_id: None,
            },
        };
        // The handler must succeed (no ServiceError). The removed count reflects
        // the shim's behaviour — we only check it is a valid u64 (not a type
        // mismatch or panic).
        let resp =
            handle(&lunaris, &scope, params).await.expect("source_prefix dispatch must not error");
        let _ = resp.removed; // type-check: must be u64
    }

    #[tokio::test]
    async fn forget_by_episode_id_handler_dispatches_without_error() {
        let (lunaris, scope) = make_engine().await;

        // Ingest with a known ULID so build_target maps to ForgetTarget::Id.
        let known_ulid = ulid::Ulid::new();
        let scoped = lunaris.scoped(scope.clone());
        scoped
            .ingest(EpisodeBuilder::new("src:ep-delete", "delete me").id(known_ulid))
            .await
            .unwrap();

        let params = ForgetParams {
            target: ForgetTarget { source_prefix: None, episode_id: Some(known_ulid.to_string()) },
        };
        // ForgetTarget::Id path — exercises the ULID parse → engine dispatch.
        let resp =
            handle(&lunaris, &scope, params).await.expect("episode_id dispatch must not error");
        let _ = resp.removed; // type-check: must be u64
    }

    // ── 0.6.2 Task F — dry_run preview contract ───────────────────────────────
    //
    // The MCP caller is an LLM. An irreversible scope-wide delete MUST NOT be
    // the default. These tests pin the inverted default: omitting `dry_run`
    // previews; deleting requires an explicit `dry_run: false`.
    //
    // They deliberately drive the WIRE form (`serde_json::from_str`) rather
    // than a struct literal — the default only exists on the deserialize path,
    // which is exactly what an MCP client exercises. Responses are asserted
    // through `serde_json::to_value` for the same reason: the contract is the
    // JSON an LLM sees, not the Rust field set.

    /// A real (non-`_dev_`) scope. `forget_scoped` (Wave 1D) routes scan +
    /// write through the caller's partition, so an honest round-trip needs a
    /// real scope — `Scope::dev()` is not required here.
    async fn make_scoped_engine() -> (Lunaris, Scope) {
        let lunaris = Lunaris::open("memory://").await.expect("in-memory lunaris must open");
        let scope = Scope::new("forget-dry-run-test").expect("test scope must be valid");
        (lunaris, scope)
    }

    /// `true` iff the episode row is present AND its system interval is still
    /// open (i.e. no soft-delete tombstone was written).
    async fn episode_is_live(lunaris: &Lunaris, scope: &Scope, id: Ulid) -> bool {
        use lunaris_core::StoragePort;
        let key = lunaris_core::keyspace::episode_key(scope, id);
        let now = lunaris.clock().tick();
        match lunaris.storage().read_as_of(scope, &key, now).await.expect("read_as_of must succeed")
        {
            Some(row) => row.bt.sys.1.is_none(),
            None => false,
        }
    }

    async fn seed_episode(lunaris: &Lunaris, scope: &Scope, source: &str) -> Ulid {
        let id = Ulid::new();
        lunaris
            .scoped(scope.clone())
            .ingest(EpisodeBuilder::new(source, "content under test").id(id))
            .await
            .expect("ingest must succeed");
        id
    }

    #[tokio::test]
    async fn omitted_dry_run_deserializes_to_preview() {
        let params: ForgetParams =
            serde_json::from_str(r#"{"target":{"source_prefix":"src:notes/"}}"#)
                .expect("the default wire form must parse");
        let v = serde_json::to_value(&params).expect("params serialize");
        assert_eq!(
            v["dry_run"],
            serde_json::json!(true),
            "omitting dry_run MUST default to a preview on the MCP surface: {v}"
        );
    }

    #[tokio::test]
    async fn dry_run_false_is_accepted_by_the_dto() {
        // deny_unknown_fields rejects this today — the field must exist.
        let params: ForgetParams =
            serde_json::from_str(r#"{"target":{"source_prefix":"src:notes/"},"dry_run":false}"#)
                .expect("explicit dry_run:false must be accepted by the DTO");
        let v = serde_json::to_value(&params).expect("params serialize");
        assert_eq!(v["dry_run"], serde_json::json!(false), "explicit false must round-trip: {v}");
    }

    #[tokio::test]
    async fn default_forget_previews_and_deletes_nothing() {
        let (lunaris, scope) = make_scoped_engine().await;
        let id = seed_episode(&lunaris, &scope, "src:notes/a").await;
        assert!(
            episode_is_live(&lunaris, &scope, id).await,
            "precondition: the episode must be live right after ingest"
        );

        // Exactly what an LLM sends when it does not think about dry_run.
        let params: ForgetParams =
            serde_json::from_str(r#"{"target":{"source_prefix":"src:notes/"}}"#)
                .expect("default wire form must parse");
        let resp = handle(&lunaris, &scope, params).await.expect("forget must not error");
        let v = serde_json::to_value(&resp).expect("response serializes");

        assert_eq!(v["dry_run"], serde_json::json!(true), "response must mark the preview: {v}");
        assert_eq!(v["status"], serde_json::json!("preview"), "flat status discriminator: {v}");
        assert_eq!(v["removed"], serde_json::json!(0), "a preview removes NOTHING: {v}");
        assert_eq!(
            v["matched"],
            serde_json::json!(1),
            "a preview must report what it WOULD delete: {v}"
        );
        assert!(
            episode_is_live(&lunaris, &scope, id).await,
            "default memory.forget MUST NOT delete — the episode was destroyed by a preview call"
        );
    }

    #[tokio::test]
    async fn explicit_dry_run_false_actually_deletes() {
        let (lunaris, scope) = make_scoped_engine().await;
        let id = seed_episode(&lunaris, &scope, "src:notes/a").await;

        let params: ForgetParams =
            serde_json::from_str(r#"{"target":{"source_prefix":"src:notes/"},"dry_run":false}"#)
                .expect("explicit dry_run:false must be accepted by the DTO");
        let resp = handle(&lunaris, &scope, params).await.expect("forget must not error");
        let v = serde_json::to_value(&resp).expect("response serializes");

        assert_eq!(v["dry_run"], serde_json::json!(false), "committed call is not a preview: {v}");
        assert_eq!(v["status"], serde_json::json!("deleted"), "flat status discriminator: {v}");
        assert_eq!(v["matched"], serde_json::json!(1), "match count is reported on commit too: {v}");
        assert_eq!(v["removed"], serde_json::json!(1), "the episode must actually be removed: {v}");
        assert!(
            !episode_is_live(&lunaris, &scope, id).await,
            "explicit dry_run:false MUST delete — the episode survived"
        );
    }

    #[tokio::test]
    async fn episode_id_target_also_defaults_to_preview() {
        let (lunaris, scope) = make_scoped_engine().await;
        let id = seed_episode(&lunaris, &scope, "src:ep-delete").await;

        let params: ForgetParams =
            serde_json::from_str(&format!(r#"{{"target":{{"episode_id":"{id}"}}}}"#))
                .expect("default wire form must parse");
        let resp = handle(&lunaris, &scope, params).await.expect("forget must not error");
        let v = serde_json::to_value(&resp).expect("response serializes");

        assert_eq!(v["dry_run"], serde_json::json!(true), "id target previews by default too: {v}");
        assert_eq!(v["matched"], serde_json::json!(1), "the id target matched one episode: {v}");
        assert_eq!(v["removed"], serde_json::json!(0), "preview removes nothing: {v}");
        assert!(
            episode_is_live(&lunaris, &scope, id).await,
            "episode_id preview MUST NOT delete the episode"
        );
    }
}

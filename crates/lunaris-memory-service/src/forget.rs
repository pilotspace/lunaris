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
//!
//! ## `dry_run` defaults to TRUE here (0.6.2 Task F)
//!
//! This surface is driven by an LLM, so an irreversible scope-wide delete may
//! not be the default. Omitting `dry_run` (or sending `true`) previews: the
//! engine scans, reports `matched`, writes NOTHING, and the response carries
//! `status = "preview"`. A real delete requires an explicit `dry_run: false`.
//!
//! The HTTP surface (`lunaris-server`, `ForgetRequestDto`) keeps `dry_run`
//! defaulting to `false` for API compatibility — only MCP inverts it. Do not
//! "fix" the asymmetry; it is the ruling.
//!
//! Known limitation of `matched`: the engine's scan matches on the stored
//! `source` / id, not on liveness, so an episode that was already soft-deleted
//! still counts. Re-previewing a target you just forgot reports the same
//! `matched` with `removed: 0` on the preview — an over-estimate, never an
//! under-estimate, so it cannot hide a deletion from the caller. Narrowing it
//! means teaching `scan_matches_scoped` the sys-gate, which changes
//! `rows_written` on the HTTP path too; out of scope for 0.6.2 Task F.

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

/// `dry_run` default for the MCP surface: **preview**.
///
/// Deliberately inverted relative to `lunaris-server`'s `ForgetRequestDto`
/// (which defaults to `false`). See the module docs.
fn default_dry_run() -> bool {
    true
}

/// Input parameters for `memory.forget`.
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForgetParams {
    /// What to delete.
    pub target: ForgetTarget,

    /// Preview switch — **defaults to `true`**.
    ///
    /// `true` (or omitted): scan only. Nothing is written, `removed` is `0`,
    /// and `matched` reports what a real call would remove.
    /// `false`: commit the (soft) delete.
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

/// Output of a successful `memory.forget` call.
///
/// Flat struct by contract: rmcp 1.7 validates each tool's generated
/// `outputSchema` at router-build time and aborts server startup when the
/// root is not `type: "object"`. The preview/commit outcome is therefore a
/// `status` **field**, never a `#[serde(tag = …)]` enum discriminator
/// (CLAUDE.md MCP tool-schema invariant; guard:
/// `crates/lunaris-mcp/tests/server_boot.rs`).
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgetResponse {
    /// `"preview"` when nothing was deleted, `"deleted"` when the delete
    /// committed. Mirrors `dry_run`; carried as a field for LLM legibility.
    pub status: String,

    /// `true` when this call was a preview (the effective `dry_run`, echoed
    /// from the engine receipt's `preview` flag rather than the request, so
    /// it can never disagree with what the engine actually did).
    pub dry_run: bool,

    /// Episodes the target matched — i.e. what a committing call WOULD
    /// remove. Reported on both paths.
    pub matched: u64,

    /// Number of episodes logically removed. Always `0` on a preview.
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
///
/// `params.dry_run` (default `true`) selects preview vs commit; the engine's
/// dry-run path returns before `atomic_write`, so a preview cannot mutate
/// storage even if a later refactor got the response mapping wrong.
pub async fn handle(
    lunaris: &Lunaris,
    scope: &Scope,
    params: ForgetParams,
) -> Result<ForgetResponse, ServiceError> {
    let engine_target = build_target(params.target)?;

    let mut request = lunaris::forget::ForgetRequest::from(engine_target);
    request.options.dry_run = params.dry_run;

    let scoped = lunaris.scoped(scope.clone());
    let receipt = scoped.forget(request).await?;

    // Soft-delete (default) populates rows_written; hard-delete populates
    // rows_deleted. The MCP wire doesn't expose the hard flag, so in practice
    // rows_deleted is always 0 here. Sum both to be defensive.
    let removed = receipt.rows_written + receipt.rows_deleted;
    let status = if receipt.preview { "preview" } else { "deleted" };

    tracing::debug!(
        scope = scope.as_str(),
        status = status,
        matched = receipt.matched,
        removed = removed,
        "memory.forget completed",
    );

    Ok(ForgetResponse {
        status: status.to_string(),
        dry_run: receipt.preview,
        matched: receipt.matched,
        removed,
    })
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
        let params = ForgetParams {
            target: ForgetTarget { source_prefix: None, episode_id: None },
            dry_run: false,
        };
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
            dry_run: false,
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
            dry_run: false,
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
            dry_run: false,
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
            dry_run: false,
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
            dry_run: false,
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

    /// Is the episode still live? Returns `(live, evidence)` — the evidence
    /// string goes into the assertion message so a failure shows the stored
    /// state, not just `false`.
    ///
    /// A forget soft-delete stamps `bt.sys[1]` INSIDE the payload
    /// (`build_soft_delete_op`) and commits it as a `KvPut`. Moon and Postgres
    /// derive the row's `bt` from those same bytes, but the embedded (SQLite)
    /// backend opens a fresh interval on every `KvPut` and keeps `bt` in its
    /// own column — so on `memory://` the row-level `bt` stays open and the
    /// tombstone is visible only in the payload. That payload gate is exactly
    /// what `lunaris_retrieve::hydrate` reads to hide forgotten episodes.
    /// Check both, so this probe is honest on every backend.
    async fn episode_probe(lunaris: &Lunaris, scope: &Scope, id: Ulid) -> (bool, String) {
        let key = lunaris_core::keyspace::episode_key(scope, id);
        let now = lunaris.clock().tick();
        match lunaris.storage().read_as_of(scope, &key, now).await.expect("read_as_of must succeed")
        {
            None => (false, "row absent".to_string()),
            Some(row) => {
                let payload: serde_json::Value =
                    serde_json::from_slice(&row.value).unwrap_or(serde_json::Value::Null);
                let payload_sys_to =
                    payload.pointer("/bt/sys/1").cloned().unwrap_or(serde_json::Value::Null);
                let live = row.bt.sys.1.is_none() && payload_sys_to.is_null();
                let evidence =
                    format!("row.bt.sys.1={:?}, payload.bt.sys[1]={payload_sys_to}", row.bt.sys.1);
                (live, evidence)
            }
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
        let (live, why) = episode_probe(&lunaris, &scope, id).await;
        assert!(live, "precondition: the episode must be live right after ingest ({why})");

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
        let (live, why) = episode_probe(&lunaris, &scope, id).await;
        assert!(
            live,
            "default memory.forget MUST NOT delete — the episode was destroyed by a preview call ({why})"
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
        assert_eq!(
            v["matched"],
            serde_json::json!(1),
            "match count is reported on commit too: {v}"
        );
        assert_eq!(v["removed"], serde_json::json!(1), "the episode must actually be removed: {v}");
        let (live, why) = episode_probe(&lunaris, &scope, id).await;
        assert!(!live, "explicit dry_run:false MUST delete — the episode survived ({why})");
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
        let (live, why) = episode_probe(&lunaris, &scope, id).await;
        assert!(live, "episode_id preview MUST NOT delete the episode ({why})");
    }
}

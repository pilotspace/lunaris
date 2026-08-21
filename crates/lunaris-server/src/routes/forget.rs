//! Plan 05-01 — `POST /v1/forget` (PROTO-04 + Plan 04-05 surface).
//!
//! Body: `ForgetRequestDto { target, hard, dry_run, confirmation_token }`.
//! Response: `ForgetReceipt` JSON (Plan 04-05 shape).
//!
//! ## D-21 two-step hard-delete safety rail
//!
//! `ForgetConfirmation` is opaque to external callers — it can ONLY be
//! constructed by `Lunaris::confirm_hard_forget(receipt)` (private inner
//! field per `crates/lunaris/src/forget.rs:101`). The HTTP boundary
//! therefore carries the entire prior `ForgetReceipt` JSON in the
//! `confirmation_token` field; the handler deserializes it, calls
//! `confirm_hard_forget` to mint the typed token, then attaches it to the
//! request before dispatching `forget`.
//!
//! Missing token on `hard=true` → server synthesizes the typed
//! `ValidateError::ConfirmationRequired` error which `map_error` translates
//! to HTTP 428 Precondition Required (the same shape Plan 04-05 raises in
//! the Rust API).

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use lunaris::{ForgetReceipt, ForgetTarget};

use crate::dto::ForgetRequestDto;
use crate::metrics::metrics;
use crate::middleware::auth::AuthClaims;
use crate::middleware::error::map_error;
use crate::state::AppState;

pub async fn forget_handler(
    State(state): State<AppState>,
    Extension(claims): Extension<AuthClaims>,
    Json(req): Json<ForgetRequestDto>,
) -> Response {
    let target = req.target.clone();
    // RFC 0001 Wave 1E: use claims.scope for metrics labels.
    let scope_str = claims.scope.as_str();

    // Plan 05-05 OPS-06 — `target_kind` + `hard` labels per CONTEXT.md D-25.
    // Cardinality: target_kind ∈ {id, scope, before} (3 values); hard ∈
    // {true, false} (2 values). Combined per-scope_str cardinality bounded by
    // tokens-file size × 6.
    let target_kind = match &target {
        ForgetTarget::Id(_) => "id",
        ForgetTarget::Scope(_) => "scope",
        ForgetTarget::Before(_) => "before",
        _ => "unknown",
    };
    let hard_label = if req.hard { "true" } else { "false" };

    let mut request = lunaris::forget::ForgetRequest::from(target);

    if req.dry_run {
        request.options.dry_run = true;
    }

    if req.hard {
        request.options.hard = true;
        // D-21 — caller MUST first run dry_run, then re-issue with the
        // serialized prior `ForgetReceipt` JSON in the `confirmation_token`
        // body field. The server deserializes + calls confirm_hard_forget
        // to mint the typed `ForgetConfirmation` token (private constructor
        // per crates/lunaris/src/forget.rs:101).
        let receipt_json = match req.confirmation_token.as_deref() {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => {
                metrics()
                    .forget_total
                    .with_label_values(&[scope_str, target_kind, hard_label])
                    .inc();
                return map_error(lunaris_core::LunarisError::Validate(
                    lunaris_core::ValidateError::ConfirmationRequired(
                        "hard-delete requires confirmation_token (serialized prior ForgetReceipt JSON)".to_string(),
                    ),
                ));
            }
        };
        let prior_receipt: ForgetReceipt = match serde_json::from_str(&receipt_json) {
            Ok(r) => r,
            Err(e) => {
                metrics()
                    .forget_total
                    .with_label_values(&[scope_str, target_kind, hard_label])
                    .inc();
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "invalid_confirmation_token",
                        "message": format!("confirmation_token must be a serialized ForgetReceipt: {e}"),
                    })),
                )
                    .into_response();
            }
        };
        let token = match state.lunaris.confirm_hard_forget(prior_receipt).await {
            Ok(t) => t,
            Err(e) => {
                metrics()
                    .forget_total
                    .with_label_values(&[scope_str, target_kind, hard_label])
                    .inc();
                return map_error(e);
            }
        };
        request.options.confirmation_token = Some(token);
    }

    // W1.1 (2026-08-21) — the route runs the SCOPED pipeline. Until this
    // commit the handler called the deprecated bare `Lunaris::forget`, whose
    // scan / read / write all route through `Scope::dev()`; every real tenant
    // got `200 OK` with `rows_written = 0` and nothing was ever deleted. The
    // JWT `tenant` claim is the only source of truth for the partition key
    // (CLAUDE.md "HTTP DTO discipline"), so `claims.scope` is what binds here.
    let result = state.lunaris.scoped(claims.scope.clone()).forget(request).await;

    // Always increment forget_total — both success + error contribute to the
    // counter. error_total increment lives inside map_error (W-11 fix).
    metrics().forget_total.with_label_values(&[scope_str, target_kind, hard_label]).inc();

    match result {
        // W1.1 — a single-ULID target that matched nothing inside the caller's
        // own partition is `404 Not Found`, not `200 OK` with a zero receipt.
        // A 200 here is exactly the failure mode this task closes: the caller
        // cannot distinguish "deleted" from "your token cannot see it".
        // 404 rather than 403 is deliberate — a cross-scope ULID must be
        // indistinguishable from a non-existent one, so the status must not
        // confirm that the id exists in somebody else's partition.
        // Bulk predicates (`Scope` / `Before`) keep 200-with-zero: matching
        // nothing is a legitimate outcome for a range delete, not a miss.
        Ok(receipt) if receipt.matched == 0 && target_kind == "id" => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "not_found",
                "message": "no episode with that id is visible in this token's scope",
            })),
        )
            .into_response(),
        Ok(receipt) => (StatusCode::OK, Json(receipt)).into_response(),
        Err(e) => map_error(e),
    }
}

//! Plan 05-01 — LunarisError → HTTP status + JSON envelope.
//!
//! Analog: `crates/lunaris/src/forget.rs:30-32` typed-variant pattern.
//! D-21 / Plan 04-05 ConfirmationRequired → 428 Precondition Required so a
//! client can re-issue the dry_run + token flow.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use lunaris_core::{LunarisError, StorageError, ValidateError};

/// Map every `LunarisError` variant to the appropriate HTTP status + JSON
/// envelope. Auth/rate-limit middleware emits 401/403/429 BEFORE this map runs,
/// so this only covers business-logic errors.
pub fn map_error(err: LunarisError) -> Response {
    let (status, code, message) = match &err {
        LunarisError::Validate(ValidateError::ConfirmationRequired(_)) => (
            StatusCode::PRECONDITION_REQUIRED,
            "confirmation_required",
            err.to_string(),
        ),
        LunarisError::Validate(_) => (StatusCode::BAD_REQUEST, "validate", err.to_string()),
        LunarisError::Storage(StorageError::NotSupported(_)) => (
            StatusCode::NOT_IMPLEMENTED,
            "not_supported",
            err.to_string(),
        ),
        LunarisError::Storage(StorageError::UnsupportedScheme(_)) => (
            StatusCode::BAD_REQUEST,
            "unsupported_scheme",
            err.to_string(),
        ),
        LunarisError::Storage(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage",
            err.to_string(),
        ),
        LunarisError::Extract(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "extract",
            err.to_string(),
        ),
        LunarisError::Retrieve(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "retrieve",
            err.to_string(),
        ),
        LunarisError::Consolidate(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "consolidate",
            err.to_string(),
        ),
    };
    (
        status,
        Json(serde_json::json!({ "error": code, "message": message })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn confirmation_required_maps_to_428() {
        let err = LunarisError::Validate(ValidateError::ConfirmationRequired("test".into()));
        let resp = map_error(err);
        assert_eq!(resp.status(), StatusCode::PRECONDITION_REQUIRED);
        let body_bytes = to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(v["error"], "confirmation_required");
    }

    #[tokio::test]
    async fn storage_not_supported_maps_to_501() {
        let err = LunarisError::Storage(StorageError::NotSupported("nope"));
        let resp = map_error(err);
        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[tokio::test]
    async fn validate_temporal_maps_to_400() {
        let err = LunarisError::Validate(ValidateError::Temporal);
        let resp = map_error(err);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn storage_backend_maps_to_500() {
        let err = LunarisError::Storage(StorageError::Backend("boom".into()));
        let resp = map_error(err);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}

//! `GET /v1/episode/{id}` — fetch a single episode by ULID, or 404 if absent.
//!
//! ## Behaviour
//!
//! | Condition                         | Response                                      |
//! |-----------------------------------|-----------------------------------------------|
//! | `{id}` is not a valid ULID        | `400 { "error": "invalid_episode_id", ... }`  |
//! | Episode exists in caller's scope  | `200 application/json` — episode value body   |
//! | Episode absent from caller's scope| `404 { "error": "episode_not_found", ... }`   |
//! | Storage error                     | `500` via `map_error`                         |
//!
//! The JWT `tenant` claim is the exclusive source of truth for the partition
//! scope — no wire-side `scope` field is accepted (DTO carries
//! `#[serde(deny_unknown_fields)]` per the HTTP DTO discipline).
//!
//! ## Key construction
//!
//! Uses `lunaris_core::keyspace::episode_key(&scope, ulid)` — the canonical
//! `lunaris:{scope}:episode:{ulid}` KV key. This helper lives in `lunaris-core`
//! (Wave 2.5B) and is the ONLY place the format is authoritative.
//!
//! ## Storage read
//!
//! Calls `StoragePort::read_as_of(&scope, &key, clock.tick())` where the HLC
//! tick serves as "latest visible" — the same pattern used by `forget.rs`.
//! `read_as_of` returns `Ok(Some(row))` on hit, `Ok(None)` on miss.

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ulid::Ulid;

use lunaris_core::keyspace::episode_key;

use crate::middleware::auth::AuthClaims;
use crate::middleware::error::map_error;
use crate::state::AppState;

/// Handler for `GET /v1/episode/{id}`.
///
/// Parses `{id}` as a [`Ulid`], looks up the episode KV row in the caller's
/// JWT-bound scope, and returns the stored JSON value or a typed 404.
pub async fn episode_handler(
    State(state): State<AppState>,
    Extension(claims): Extension<AuthClaims>,
    Path(id_str): Path<String>,
) -> Response {
    // Step 1 — parse the ULID path parameter.
    let ulid = match Ulid::from_string(&id_str) {
        Ok(u) => u,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_episode_id",
                    "message": format!(
                        "'{id_str}' is not a valid ULID; \
                         expected 26-character Crockford base-32 string"
                    ),
                })),
            )
                .into_response();
        }
    };

    // Step 2 — build the canonical KV key for this episode in the caller's scope.
    let key = episode_key(&claims.scope, ulid);

    // Step 3 — read the latest visible row. `clock.tick()` is the conventional
    // "now" HLC used by forget.rs and the verify worker — it mints a monotonic
    // timestamp that is >= any previously committed write.
    let as_of = state.lunaris.clock().tick();
    let storage = state.lunaris.storage();

    match storage.read_as_of(&claims.scope, &key, as_of).await {
        Ok(Some(row)) => {
            // The stored value bytes are JSON — decode to serde_json::Value so
            // the response body is idiomatic JSON (not a double-escaped string).
            let value: serde_json::Value = serde_json::from_slice(&row.value).unwrap_or_else(|_| {
                // If the bytes aren't valid JSON (shouldn't happen in a
                // well-formed store), fall back to a base64-safe string
                // representation rather than failing the request.
                serde_json::Value::String(String::from_utf8_lossy(&row.value).to_string())
            });
            (StatusCode::OK, Json(value)).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "episode_not_found",
                "message": format!("no episode {ulid} in this scope"),
            })),
        )
            .into_response(),
        Err(e) => map_error(lunaris_core::LunarisError::Storage(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ULID parse branch tests (no storage needed) ───────────────────────────

    /// A well-formed 26-character Crockford base-32 string parses successfully.
    #[test]
    fn ulid_parse_valid_string() {
        let id = "01HZZZZZZZZZZZZZZZZZZZZZZZ";
        let result = Ulid::from_string(id);
        assert!(result.is_ok(), "expected Ok for valid ULID string");
    }

    /// An empty string is not a valid ULID — the handler would return 400.
    #[test]
    fn ulid_parse_empty_is_err() {
        assert!(Ulid::from_string("").is_err());
    }

    /// A plain UUID (with hyphens) is not a Crockford base-32 ULID.
    #[test]
    fn ulid_parse_uuid_format_is_err() {
        assert!(Ulid::from_string("550e8400-e29b-41d4-a716-446655440000").is_err());
    }

    /// Strings shorter than 26 characters are rejected.
    #[test]
    fn ulid_parse_short_string_is_err() {
        assert!(Ulid::from_string("TOOSHORT").is_err());
        assert!(Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FA").is_err()); // 25 chars — one short
    }

    /// Strings with characters outside the Crockford base-32 alphabet are rejected.
    /// The alphabet is 0-9 and A-Z minus I, L, O, U (case-insensitive in the ulid crate).
    /// Characters like '-' or '+' are not in the alphabet.
    #[test]
    fn ulid_parse_invalid_chars_is_err() {
        // Hyphens are not valid Crockford base-32 characters.
        assert!(Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5F--").is_err());
        // A string with '!' is clearly not a valid ULID.
        assert!(Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5F!!").is_err());
        // Valid 26-char Crockford base-32 ULID — this MUST succeed (regression pin).
        assert!(Ulid::from_string("01BX5ZZKBKACTAV9WEVGEMMVS0").is_ok());
    }

    // ── Key construction test ─────────────────────────────────────────────────

    /// episode_key produces the canonical lunaris:{scope}:episode:{ulid} format.
    #[test]
    fn episode_key_canonical_format() {
        use lunaris_core::Scope;
        use lunaris_core::keyspace::episode_key;

        let scope = Scope::new("acme.agent-1").unwrap();
        let ulid = Ulid::from_string("01HZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
        let key = episode_key(&scope, ulid);
        assert_eq!(
            key,
            b"lunaris:acme.agent-1:episode:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
        );
    }

    /// Keys for the same ULID in different scopes must differ (no cross-scope bleed).
    #[test]
    fn episode_key_differs_across_scopes() {
        use lunaris_core::Scope;
        use lunaris_core::keyspace::episode_key;

        let scope_a = Scope::new("tenant-a").unwrap();
        let scope_b = Scope::new("tenant-b").unwrap();
        let ulid = Ulid::from_string("01HZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
        assert_ne!(episode_key(&scope_a, ulid), episode_key(&scope_b, ulid));
    }
}

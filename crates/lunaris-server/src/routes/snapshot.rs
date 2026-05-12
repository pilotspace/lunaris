//! Plan 05-01 — `GET /v1/snapshot/{lsn}` (PROTO-01 + D-03).
//!
//! Streams every primitive at the given Lsn as newline-delimited JSON.
//! One JSON object per line; client parses incrementally.
//!
//! Path param shape: `wall_ms.counter[.node_id]` decimal triple. The
//! `node_id` is optional (defaults to 0 — single-node v0). Examples:
//!   `/v1/snapshot/1714000000.5`
//!   `/v1/snapshot/1714000000.5.0`
//!
//! NDJSON streaming uses `axum::body::Body::from_stream` over a futures
//! adapter on `StoragePort::scan_range(b"", as_of=Some(<hlc>))`. The empty
//! prefix walks every key visible at the snapshot.

use std::convert::Infallible;

use axum::body::Body;
use axum::extract::{Extension, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;

use lunaris_core::Hlc;

use crate::middleware::auth::AuthClaims;
use crate::middleware::error::map_error;
use crate::state::AppState;

/// Plan 05-05 W-11 fix — handler signature carries `Extension<AuthClaims>` so
/// every verb route in the workspace uses the same pattern. The snapshot verb
/// does NOT yet carry a dedicated Prometheus metric (the per-snapshot QPS is
/// not in the D-25 catalogue); the `tenant` Auth claim is reserved for a
/// future SNAP-* metric without a signature break.
pub async fn snapshot_handler(
    State(state): State<AppState>,
    Extension(claims): Extension<AuthClaims>,
    Path(lsn_str): Path<String>,
) -> Response {
    let hlc = match parse_hlc(&lsn_str) {
        Ok(h) => h,
        Err(msg) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({
                    "error": "invalid_lsn",
                    "message": format!("expected wall_ms.counter[.node_id]; {msg}"),
                })),
            )
                .into_response();
        }
    };

    // RFC: a future LSN is one whose wall_ms is strictly ahead of the engine's
    // current wall clock. We read "now" from the system clock (same source as
    // HlcClock::tick) to avoid minting a tick just for comparison. A past LSN
    // with zero visible rows is a valid empty snapshot (200); only a future
    // wall_ms is 404.
    let current_wall_ms = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    };
    if is_future_hlc(&hlc, current_wall_ms) {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": "snapshot_out_of_range",
                "message": format!(
                    "LSN wall_ms={} is beyond the engine clock (now={}); \
                     request a past or current snapshot",
                    hlc.wall_ms, current_wall_ms
                ),
            })),
        )
            .into_response();
    }

    let storage = state.lunaris.storage();

    // We need the stream to outlive `storage` borrow, so we acquire the stream
    // here, then move it into `Body::from_stream`. The stream borrows `storage`
    // for its lifetime — wrap the storage Arc into a 'static-keyed wrapper by
    // collecting into a Vec when the row count is bounded. For the v0 surface
    // we materialize the snapshot fully (single-tenant; bounded by storage
    // size) — Plan 05-05 may add `?limit=N` for streaming-bounded responses.
    let pairs: Vec<Result<(Bytes, Bytes), lunaris_core::StorageError>> =
        // RFC 0001 Wave 1E: use the JWT-bound scope so callers only see their
        // own partition's data. Wave 1B/1C will enforce this at the storage
        // level; at the HTTP layer the scope is already authoritative.
        match storage.scan_range(&claims.scope, b"", Some(hlc)).await {
            Ok(mut s) => {
                let mut acc = Vec::new();
                while let Some(item) = s.next().await {
                    acc.push(item);
                }
                acc
            }
            Err(e) => return map_error(lunaris_core::LunarisError::Storage(e)),
        };

    let body_stream = futures::stream::iter(pairs).map(|item| match item {
        Ok((k, v)) => {
            let line = serde_json::json!({
                "key": String::from_utf8_lossy(&k).to_string(),
                "value": serde_json::from_slice::<serde_json::Value>(&v)
                    .unwrap_or(serde_json::Value::String(
                        String::from_utf8_lossy(&v).to_string(),
                    )),
            });
            let mut buf = serde_json::to_vec(&line).unwrap_or_else(|_| b"{}".to_vec());
            buf.push(b'\n');
            Ok::<Bytes, Infallible>(Bytes::from(buf))
        }
        Err(e) => {
            let err = serde_json::json!({"error": "scan_error", "message": e.to_string()});
            let mut buf = serde_json::to_vec(&err).unwrap_or_default();
            buf.push(b'\n');
            Ok(Bytes::from(buf))
        }
    });

    let body = Body::from_stream(body_stream);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(body)
        .expect("build NDJSON response")
}

/// Returns `true` when `requested.wall_ms` is *strictly* in the future relative
/// to `current_wall_ms`.
///
/// A past or present LSN — including one in the same wall-millis with a higher
/// counter — is NOT a future LSN. Only `requested.wall_ms > current_wall_ms`
/// triggers 404; an LSN with a wall clock in the past but no visible rows is a
/// valid empty snapshot (200 + empty NDJSON).
///
/// Factored out for deterministic unit testing without real-time dependency.
#[inline]
pub(crate) fn is_future_hlc(requested: &Hlc, current_wall_ms: u64) -> bool {
    requested.wall_ms > current_wall_ms
}

fn parse_hlc(s: &str) -> Result<Hlc, &'static str> {
    let parts: Vec<&str> = s.split('.').collect();
    let (wall_str, counter_str, node_str) = match parts.as_slice() {
        [w, c] => (*w, *c, "0"),
        [w, c, n] => (*w, *c, *n),
        _ => return Err("expected 2 or 3 dot-separated decimal components"),
    };
    let wall_ms: u64 = wall_str.parse().map_err(|_| "wall_ms not a u64")?;
    let counter: u32 = counter_str.parse().map_err(|_| "counter not a u32")?;
    let node_id: u16 = node_str.parse().map_err(|_| "node_id not a u16")?;
    Ok(Hlc { wall_ms, counter, node_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hlc_two_part() {
        let h = parse_hlc("1714000000.5").expect("ok");
        assert_eq!(h.wall_ms, 1714000000);
        assert_eq!(h.counter, 5);
        assert_eq!(h.node_id, 0);
    }

    #[test]
    fn parse_hlc_three_part() {
        let h = parse_hlc("1714000000.5.7").expect("ok");
        assert_eq!(h.node_id, 7);
    }

    #[test]
    fn parse_hlc_rejects_non_numeric() {
        assert!(parse_hlc("abc.5").is_err());
        assert!(parse_hlc("1.x").is_err());
    }

    #[test]
    fn parse_hlc_rejects_single_part() {
        assert!(parse_hlc("1714000000").is_err());
    }

    // ── is_future_hlc predicate tests ────────────────────────────────────────

    /// A wall_ms strictly greater than current is a future LSN → 404.
    #[test]
    fn is_future_hlc_strictly_greater_wall_ms_is_future() {
        let hlc = Hlc { wall_ms: 2_000_000_000, counter: 0, node_id: 0 };
        assert!(is_future_hlc(&hlc, 1_000_000_000));
    }

    /// A wall_ms equal to current is NOT future (same-millisecond LSN is valid).
    #[test]
    fn is_future_hlc_equal_wall_ms_is_not_future() {
        let hlc = Hlc { wall_ms: 1_000_000_000, counter: 99, node_id: 0 };
        assert!(!is_future_hlc(&hlc, 1_000_000_000));
    }

    /// A wall_ms less than current is NOT future (past snapshot → valid empty 200).
    #[test]
    fn is_future_hlc_past_wall_ms_is_not_future() {
        let hlc = Hlc { wall_ms: 500_000_000, counter: 0, node_id: 0 };
        assert!(!is_future_hlc(&hlc, 1_000_000_000));
    }

    /// wall_ms=0 (Hlc::ZERO) is never future regardless of current_wall_ms.
    #[test]
    fn is_future_hlc_zero_hlc_is_never_future() {
        let hlc = Hlc::ZERO;
        assert!(!is_future_hlc(&hlc, 0));
        assert!(!is_future_hlc(&hlc, 1_000_000_000));
        assert!(!is_future_hlc(&hlc, u64::MAX));
    }

    /// u64::MAX wall_ms is always future (unless current is also u64::MAX).
    #[test]
    fn is_future_hlc_max_wall_ms_is_future_unless_current_also_max() {
        let hlc = Hlc { wall_ms: u64::MAX, counter: 0, node_id: 0 };
        assert!(is_future_hlc(&hlc, u64::MAX - 1));
        // Equal → not future
        assert!(!is_future_hlc(&hlc, u64::MAX));
    }
}

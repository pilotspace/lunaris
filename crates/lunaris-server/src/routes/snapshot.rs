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
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;

use lunaris_core::Hlc;

use crate::middleware::error::map_error;
use crate::state::AppState;

pub async fn snapshot_handler(
    State(state): State<AppState>,
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

    let storage = state.lunaris.storage();

    // We need the stream to outlive `storage` borrow, so we acquire the stream
    // here, then move it into `Body::from_stream`. The stream borrows `storage`
    // for its lifetime — wrap the storage Arc into a 'static-keyed wrapper by
    // collecting into a Vec when the row count is bounded. For the v0 surface
    // we materialize the snapshot fully (single-tenant; bounded by storage
    // size) — Plan 05-05 may add `?limit=N` for streaming-bounded responses.
    let pairs: Vec<Result<(Bytes, Bytes), lunaris_core::StorageError>> =
        match storage.scan_range(b"", Some(hlc)).await {
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
    Ok(Hlc {
        wall_ms,
        counter,
        node_id,
    })
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
}

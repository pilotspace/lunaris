//! Raw-RESP `HOTKEYS COUNT <n>` — Moon's SpaceSaving hot-key sketch
//! (ADD task `hotkeys-observability`, contract v1).
//!
//! The typed moon SDK has no hotkeys wrapper (same situation as the kv.rs
//! HSCAN escape hatch and navigate.rs FT.NAVIGATE), so we issue the command
//! over the shared `MultiplexedConnection` and parse the
//! `[[key, sampled_count], …]` array reply ourselves.

use lunaris_core::error::StorageError;
use lunaris_core::storage::types::HotKey;

use crate::client::{MoonClient, redis_err};

/// Moon's sketch capacity — `HOTKEYS COUNT` outside 1..=128 is a server
/// error ("COUNT must be an integer between 1 and 128"), so the port impl
/// clamps instead of surfacing it (§1 Reject row 4).
const MOON_HOTKEY_CAPACITY: usize = 128;

/// `StoragePort::hot_keys` body for [`crate::MoonStorage`].
pub(crate) async fn hot_keys(
    client: &MoonClient,
    count: usize,
) -> Result<Vec<HotKey>, StorageError> {
    let clamped = count.clamp(1, MOON_HOTKEY_CAPACITY);
    let mut typed = client.typed();
    let conn = typed.inner_mut();
    let reply: redis::Value = redis::cmd("HOTKEYS")
        .arg("COUNT")
        .arg(clamped)
        .query_async(conn)
        .await
        .map_err(redis_err)?;
    parse_hotkeys_reply(reply)
}

/// Parse Moon's `HOTKEYS` reply: an array of `[key, sampled_count]` pairs,
/// count-descending. An empty array (cold server / `MOON_NO_HOTKEYS=1` kill
/// switch) is `Ok(vec![])` — never an error. A malformed pair is a backend
/// error: the wire shape is fixed by Moon's `server_admin.rs::hotkeys`.
fn parse_hotkeys_reply(reply: redis::Value) -> Result<Vec<HotKey>, StorageError> {
    let redis::Value::Array(pairs) = reply else {
        return Err(StorageError::Backend(format!(
            "moon: unexpected HOTKEYS reply shape: {reply:?}"
        )));
    };
    let mut out = Vec::with_capacity(pairs.len());
    for pair in pairs {
        let redis::Value::Array(kv) = pair else {
            return Err(StorageError::Backend(format!(
                "moon: HOTKEYS entry is not a [key, count] pair: {pair:?}"
            )));
        };
        let [key, count] = kv.as_slice() else {
            return Err(StorageError::Backend(format!(
                "moon: HOTKEYS pair has {} elements, expected 2",
                kv.len()
            )));
        };
        let key = match key {
            redis::Value::BulkString(bytes) => bytes.clone(),
            redis::Value::SimpleString(s) => s.clone().into_bytes(),
            other => {
                return Err(StorageError::Backend(format!(
                    "moon: HOTKEYS key is not a string: {other:?}"
                )));
            }
        };
        let sampled_count = match count {
            redis::Value::Int(n) => u64::try_from(*n).unwrap_or(0),
            other => {
                return Err(StorageError::Backend(format!(
                    "moon: HOTKEYS count is not an integer: {other:?}"
                )));
            }
        };
        out.push(HotKey { key, sampled_count });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(key: &[u8], n: i64) -> redis::Value {
        redis::Value::Array(vec![redis::Value::BulkString(key.to_vec()), redis::Value::Int(n)])
    }

    #[test]
    fn parses_pairs_in_order() {
        let reply = redis::Value::Array(vec![pair(b"a", 20), pair(b"b", 5)]);
        let got = parse_hotkeys_reply(reply).expect("valid reply");
        assert_eq!(
            got,
            vec![
                HotKey { key: b"a".to_vec(), sampled_count: 20 },
                HotKey { key: b"b".to_vec(), sampled_count: 5 },
            ]
        );
    }

    #[test]
    fn empty_reply_is_ok_empty() {
        let got = parse_hotkeys_reply(redis::Value::Array(vec![])).expect("empty is legal");
        assert!(got.is_empty());
    }

    #[test]
    fn malformed_reply_is_backend_error() {
        for bad in [
            redis::Value::Int(3),
            redis::Value::Array(vec![redis::Value::Int(3)]),
            redis::Value::Array(vec![redis::Value::Array(vec![redis::Value::Int(1)])]),
        ] {
            let err = parse_hotkeys_reply(bad).expect_err("malformed must error");
            assert!(matches!(err, StorageError::Backend(_)), "got {err:?}");
        }
    }

    #[test]
    fn negative_count_clamps_to_zero() {
        let reply = redis::Value::Array(vec![pair(b"a", -3)]);
        let got = parse_hotkeys_reply(reply).expect("negative count tolerated");
        assert_eq!(got[0].sampled_count, 0);
    }
}

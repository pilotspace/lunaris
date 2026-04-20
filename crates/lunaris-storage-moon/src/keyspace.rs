//! Moon keyspace conventions for Lunaris primitives.
//!
//! Format: `lunaris:<primitive>:<ulid>`. Hash fields match the primitive's serde fields.
//! These helpers are used by the `atomic_write` fan-out and by `read_as_of` / `scan_range`.
//!
//! Plan 1 only round-trips Episode, but the keyspace helpers ship for all six primitives so
//! Phase 2 does not churn the convention.

use ulid::Ulid;

#[inline]
pub fn episode_key(id: Ulid) -> Vec<u8> {
    format!("lunaris:episode:{id}").into_bytes()
}
#[inline]
pub fn chunk_key(id: Ulid) -> Vec<u8> {
    format!("lunaris:chunk:{id}").into_bytes()
}
#[inline]
pub fn entity_key(id: Ulid) -> Vec<u8> {
    format!("lunaris:entity:{id}").into_bytes()
}
#[inline]
pub fn relation_key(id: Ulid) -> Vec<u8> {
    format!("lunaris:relation:{id}").into_bytes()
}
#[inline]
pub fn fact_key(id: Ulid) -> Vec<u8> {
    format!("lunaris:fact:{id}").into_bytes()
}
#[inline]
pub fn community_key(id: Ulid) -> Vec<u8> {
    format!("lunaris:community:{id}").into_bytes()
}

#[inline]
pub fn episode_prefix() -> &'static [u8] {
    b"lunaris:episode:"
}
#[inline]
pub fn chunk_prefix() -> &'static [u8] {
    b"lunaris:chunk:"
}
#[inline]
pub fn entity_prefix() -> &'static [u8] {
    b"lunaris:entity:"
}
#[inline]
pub fn relation_prefix() -> &'static [u8] {
    b"lunaris:relation:"
}
#[inline]
pub fn fact_prefix() -> &'static [u8] {
    b"lunaris:fact:"
}
#[inline]
pub fn community_prefix() -> &'static [u8] {
    b"lunaris:community:"
}

#[cfg(test)]
mod tests {
    use super::*;
    use ulid::Ulid;

    #[test]
    fn keyspace_format_is_stable() {
        // 26-char crockford-base32 ULID; pick a deterministic one.
        let id = Ulid::from_string("01HZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
        assert_eq!(
            episode_key(id),
            b"lunaris:episode:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
        );
        assert_eq!(
            chunk_key(id),
            b"lunaris:chunk:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
        );
        assert_eq!(
            entity_key(id),
            b"lunaris:entity:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
        );
        assert_eq!(
            relation_key(id),
            b"lunaris:relation:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
        );
        assert_eq!(
            fact_key(id),
            b"lunaris:fact:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
        );
        assert_eq!(
            community_key(id),
            b"lunaris:community:01HZZZZZZZZZZZZZZZZZZZZZZZ".to_vec()
        );
    }

    #[test]
    fn prefixes_match_key_starts() {
        let id = Ulid::new();
        let k = episode_key(id);
        assert!(k.starts_with(episode_prefix()));
        let k = chunk_key(id);
        assert!(k.starts_with(chunk_prefix()));
    }
}

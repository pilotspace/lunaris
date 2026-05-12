//! `MoonClient` — typed `moon-client` v0.1.0 SDK wrapped in a Lunaris-shaped handle.
//!
//! Phase 1.5 retrofit (STORE-09) replaces the previous hand-rolled `redis 0.32+` RESP
//! wrapper with the typed `moon-client` SDK at `/Users/tindang/workspaces/tind-repo/moon/sdk/rust/`.
//! The `moon-client::MoonClient` is `Clone` (cheap — backed by a shared
//! `redis::aio::MultiplexedConnection`), so we don't need an outer mutex; per-call we
//! `.clone()` the underlying client into a local `mut` binding and dispatch sub-clients
//! from there.
//!
//! ## URL grammar
//!
//! `moon://host:port[?ws=workspace]`
//!
//! Examples:
//!   * `moon://localhost:6390`               → host=localhost, port=6390, no workspace
//!   * `moon://moon.example.com:6390?ws=hot` → host=moon.example.com, port=6390, ws=hot
//!
//! The `moon://` scheme is the Lunaris-public face; internally we translate to
//! `redis://host:port` because Moon speaks the Redis wire protocol. The URL parser
//! rejects any non-`moon` scheme BEFORE any network IO so a malicious `redis://` URL
//! cannot exercise this code path (defense in depth — mirrors the URL dispatcher in
//! `crates/lunaris/src/open.rs`).
//!
//! The optional `ws` query parameter is recorded on the struct for later use (Phase 2
//! may multiplex by workspace); Plan 03 records it but does not act on it.

use lunaris_core::error::StorageError;
use moon::{MoonClient as TypedClient, MoonError};

/// Default Moon RESP port (matches Moon's `bin/moond` default).
pub const DEFAULT_MOON_PORT: u16 = 6390;

/// A live typed `moon-client` connection to a Moon instance, parsed from a `moon://` URL.
///
/// `Clone` is cheap — the underlying `moon_client::MoonClient` shares its
/// `redis::aio::MultiplexedConnection` via `Arc`. Each `clone()` yields an independent
/// handle into the same connection so concurrent requests do not contend on a single
/// mutex.
///
/// `Debug` is hand-implemented because `moon_client::MoonClient` does NOT impl `Debug`
/// in v0.1.0 (it would leak driver internals); we redact the inner connection.
#[derive(Clone)]
pub struct MoonClient {
    /// Resolved host from the `moon://host:port` URL.
    pub host: String,
    /// Resolved port; defaults to `DEFAULT_MOON_PORT` when omitted.
    pub port: u16,
    /// Optional workspace selector from the `?ws=...` query param.
    pub workspace: Option<String>,
    /// The typed `moon-client` SDK handle. Cheap to clone.
    pub(crate) inner: TypedClient,
}

impl std::fmt::Debug for MoonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoonClient")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("workspace", &self.workspace)
            .field("inner", &"<moon_client::MoonClient>")
            .finish()
    }
}

impl MoonClient {
    /// Parse a `moon://host:port[?ws=workspace]` URL and open a typed `moon-client`
    /// connection.
    ///
    /// Returns `StorageError::UnsupportedScheme` if the URL fails to parse OR if the
    /// scheme is anything other than `moon`. The unknown-scheme arm runs BEFORE any
    /// network IO so a malicious `redis://` URL cannot exercise the Moon code path
    /// (defense in depth — mirrors the URL dispatcher in `crates/lunaris/src/open.rs`).
    pub async fn connect(url: &str) -> Result<Self, StorageError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| StorageError::UnsupportedScheme(format!("moon parse: {e}")))?;
        if parsed.scheme() != "moon" {
            return Err(StorageError::UnsupportedScheme(parsed.scheme().into()));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| StorageError::Backend("moon URL missing host".into()))?
            .to_string();
        let port = parsed.port().unwrap_or(DEFAULT_MOON_PORT);
        let workspace = parsed.query_pairs().find(|(k, _)| k == "ws").map(|(_, v)| v.into_owned());

        // Moon speaks RESP2/RESP3 over the Redis protocol. We dial via the typed
        // moon-client SDK which internally opens a `redis::aio::MultiplexedConnection`.
        // Bulk-write bench workloads (50K-1M HSET ops) can exceed the SDK's
        // default ~60s timeout; we wrap the connect call in a 5-minute tokio
        // timeout. `TypedClient::connect_with_timeout` exists on the local
        // path-dep build of moondb but isn't in the crates.io 0.1.1 release,
        // so we use the always-available `connect` + outer `tokio::time::timeout`
        // for crates.io compatibility (semver-friendly).
        let redis_url = format!("redis://{host}:{port}");
        let connect_timeout = std::time::Duration::from_secs(300);
        let inner = tokio::time::timeout(connect_timeout, TypedClient::connect(redis_url.as_str()))
            .await
            .map_err(|_| {
                StorageError::Backend(format!(
                    "moon connect timed out after {}s",
                    connect_timeout.as_secs()
                ))
            })?
            .map_err(moon_err)?;
        let me = Self { host, port, workspace, inner };
        me.ensure_indexes().await?;
        Ok(me)
    }

    /// Cheap clone of the underlying typed client. Use one clone per concurrent task.
    pub fn typed(&self) -> TypedClient {
        self.inner.clone()
    }

    /// Idempotently create the FT indexes Lunaris uses (`chunks`, `entities`,
    /// `facts`, `communities`). Each index declares the dense `vec` field
    /// (HNSW Cosine, 768d) AND a TEXT `content` field so BM25 / HYBRID
    /// `FT.SEARCH` can score against the per-row text payload populated by
    /// `WriteOp::VectorUpsert` (Gap 9 fix 2026-04-21 —
    /// `extract_content_for_index` mirrors the Postgres tsvector convention).
    ///
    /// Per Moon's vector model (`docs/vector-search-guide.md`), HSET writes to keys
    /// matching `<prefix>` are auto-indexed by FT.SEARCH. Without this call, every
    /// HSET would land in a hash that no FT index covers — recall returns empty.
    /// Duplicate-create errors (`Index already exists`) are swallowed so reopening
    /// an existing Moon instance is a no-op.
    ///
    /// NOTE: Moon's `FT.CREATE` is fully idempotent — once an index exists with
    /// a stale schema (e.g. vector-only from before Gap 9, or missing the
    /// `valid_time` NUMERIC field from before Phase 9.1 Plan 02), `create_index`
    /// returns "already exists" and DOES NOT update the schema. After upgrading
    /// past Phase 9.1, operators must `FT.DROPINDEX chunks` once before the new
    /// schema takes effect (Moon will recreate on next `ensure_indexes` call).
    async fn ensure_indexes(&self) -> Result<(), StorageError> {
        use moon::{DistanceMetric, SchemaField, VectorIndexOptions};
        const DIM: usize = 768;
        for (name, prefix) in &[
            ("chunks", "chunks:"),
            ("entities", "entities:"),
            ("facts", "facts:"),
            ("communities", "communities:"),
        ] {
            let mut opts = VectorIndexOptions::new(DIM, DistanceMetric::Cosine)
                .prefix(*prefix)
                .field_name("vec")
                .add_field(SchemaField::Text("content".to_string()));
            // Plan 09.1-02 Task 2 — chunks gets an additional NUMERIC field on
            // `valid_time` so `Filter::ValidTimeRange` renders as
            // `@valid_time:[lo hi]` against a real indexed field. Other indices
            // (entities / facts / communities) do NOT participate in the
            // TemporalQuery axis and stay unchanged.
            if *name == "chunks" {
                opts = opts.add_field(SchemaField::Numeric("valid_time".to_string()));
                // Plan 15-01 Task 1 — source TAG field so `@source:{value}`
                // FT.SEARCH queries resolve server-side (PERF-MOON-01).
                opts = opts.add_field(SchemaField::Tag("source".to_string()));
            }
            let typed = self.inner.clone();
            match typed.vector().create_index(name, opts).await {
                Ok(_) => {}
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("Index already exists") || msg.contains("already exists") {
                        continue;
                    }
                    return Err(moon_err(e));
                }
            }
        }
        // Pre-create the well-known graph Lunaris's graph-on ingest writes
        // into (`crates/lunaris/src/ingest.rs::GRAPH_NAME = "lunaris_graph"`).
        // Moon does not auto-create graphs on first GRAPH.QUERY — `ERR graph
        // not found` would otherwise surface on first GraphNode/GraphEdge
        // write. `GRAPH.CREATE` is idempotent on Moon (returns OK either way),
        // so we don't filter the error.
        let typed = self.inner.clone();
        match typed.graph().create("lunaris_graph").await {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                if !(msg.contains("already exists") || msg.contains("Graph already exists")) {
                    return Err(moon_err(e));
                }
            }
        }
        Ok(())
    }
}

/// Map a `moon_client::MoonError` into Lunaris's `StorageError`.
///
/// We treat any reply starting with `NOSUPPORT ` (or containing "not supported") as
/// `StorageError::NotSupported`; everything else becomes `StorageError::Backend(msg)`
/// with the raw Moon reply preserved for debugging.
///
/// ## Threat note (T-01-03-02)
///
/// Raw Moon error messages may contain internal paths or schema names. In v0 we surface
/// them as-is to internal callers. Phase 5's `lunaris-server` will scrub error strings
/// before crossing the HTTP boundary.
#[inline]
pub(crate) fn moon_err(e: MoonError) -> StorageError {
    let s = e.to_string();
    if s.starts_with("NOSUPPORT") || s.contains("not supported") || s.contains("Unsupported") {
        StorageError::NotSupported("moon: command not supported on this server build")
    } else {
        StorageError::Backend(format!("moon: {s}"))
    }
}

/// Map a raw `redis::RedisError` into Lunaris's `StorageError`.
///
/// Used by the documented HSCAN escape hatch in `kv.rs` which calls a raw RESP
/// command directly because `moon-client` v0.1.0 does not yet expose a typed
/// wrapper for hash-scan iteration.
#[inline]
pub(crate) fn redis_err(e: redis::RedisError) -> StorageError {
    let s = e.to_string();
    if s.starts_with("NOSUPPORT") || s.contains("not supported") {
        StorageError::NotSupported("moon: command not supported on this server build")
    } else {
        StorageError::Backend(format!("moon: {s}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_wrong_scheme() {
        let r = MoonClient::connect("redis://localhost:6379").await;
        assert!(matches!(r, Err(StorageError::UnsupportedScheme(_))));
    }

    #[tokio::test]
    async fn rejects_garbage_url() {
        let r = MoonClient::connect("not a url").await;
        assert!(matches!(r, Err(StorageError::UnsupportedScheme(_))));
    }

    /// Plan 09.1-02 Task 2 — structural guard on `ensure_indexes`.
    /// The `chunks` FT index MUST declare `valid_time` as `SchemaField::Numeric`
    /// so `@valid_time:[lo hi]` range queries hit a real indexed field. Without
    /// the declaration the translator arm in vector.rs / keyword.rs renders
    /// valid grammar that matches 0 rows — a silent footgun.
    #[test]
    fn chunks_index_declares_valid_time_numeric() {
        let source = include_str!("client.rs");
        assert!(
            source.contains("SchemaField::Numeric(\"valid_time\""),
            "ensure_indexes must declare valid_time NUMERIC on the chunks FT index"
        );
    }

    /// Plan 15-01 Task 1 — structural guard: chunks FT index MUST declare
    /// `source` as `SchemaField::Tag` so `@source:{value}` TAG queries
    /// resolve server-side (PERF-MOON-01).
    #[test]
    fn ensure_indexes_declares_source_tag_on_chunks() {
        let source = include_str!("client.rs");
        assert!(
            source.contains("SchemaField::Tag(\"source\""),
            "ensure_indexes must declare source TAG on the chunks FT index"
        );
    }
}

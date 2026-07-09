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
//!   * `moon://localhost:6380`               → host=localhost, port=6380, no workspace
//!   * `moon://moon.example.com:6380?ws=hot` → host=moon.example.com, port=6380, ws=hot
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
pub const DEFAULT_MOON_PORT: u16 = 6380;

/// FT vector-quantization tier for Moon's `FT.CREATE … QUANTIZATION <q>`
/// clause. **v0.5.1+ Lunaris default is [`Quantization::Sq8`]** (moon-v051
/// -perf-exploit W1-1): Moon's int8 symmetric SIMD ADC + exact f16 rerank
/// sidecar (HQ-1) makes SQ8 both higher-recall AND ~2.15× faster per
/// candidate than the old TQ4 default at 768-d (our production dim) — see
/// `vendor/moon/CHANGELOG.md` [Unreleased]. [`Quantization::Tq4`] remains
/// available as the RSS-constrained escape hatch (~452 B/vec vs SQ8's
/// ~900 B/vec + 2·dim f16 sidecar).
///
/// Override per handle via the `?quant=<q>` URL parameter
/// (`moon://host:port?quant=tq4`); every index the handle creates —
/// legacy global AND per-scope — inherits the choice. Omitting `?quant=`
/// resolves to `Sq8` via [`resolve_quantization`], not the bare SDK
/// creation path — see that function's doc for the historical `None`
/// (typed-SDK, whatever Moon's own default is) escape valve, which is no
/// longer reachable through the public URL grammar.
///
/// ## Sticky-schema footgun
///
/// Like `DIM`, quantization is fixed at `FT.CREATE` time: reopening with a
/// different `?quant=` against an existing index does NOT migrate it — the
/// old tier stays in effect. Unlike the v0.3.0 note this superseded, Moon's
/// `FT.INFO` NOW reports `QUANTIZATION` (see `ft_info.rs`), so
/// [`MoonClient::connect_with_dim`] probes it for the 4 legacy global
/// indices and logs a `tracing::warn!` on drift — connect still succeeds
/// (graceful degrade, not a hard fail, mirroring the dim guardrail's
/// severity choice in reverse). See `docs/migration/0.7-quantization.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantization {
    Tq1,
    Tq2,
    Tq3,
    Tq4,
    Sq8,
}

impl Quantization {
    /// Wire rendering for the `FT.CREATE … QUANTIZATION <q>` clause.
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Tq1 => "TQ1",
            Self::Tq2 => "TQ2",
            Self::Tq3 => "TQ3",
            Self::Tq4 => "TQ4",
            Self::Sq8 => "SQ8",
        }
    }
}

impl std::str::FromStr for Quantization {
    type Err = StorageError;

    /// Case-insensitive parse of the `?quant=` URL value.
    fn from_str(s: &str) -> Result<Self, StorageError> {
        match s.to_ascii_lowercase().as_str() {
            "tq1" => Ok(Self::Tq1),
            "tq2" => Ok(Self::Tq2),
            "tq3" => Ok(Self::Tq3),
            "tq4" => Ok(Self::Tq4),
            "sq8" => Ok(Self::Sq8),
            _ => Err(StorageError::Backend(format!(
                "moon_invalid_quantization: got '{s}', expected tq1|tq2|tq3|tq4|sq8"
            ))),
        }
    }
}

/// v0.5.1+ Lunaris default quantization tier — see [`Quantization`] doc.
pub const DEFAULT_QUANTIZATION: Quantization = Quantization::Sq8;

/// Resolve the effective per-handle quantization tier from the parsed
/// `?quant=` URL value (moon-v051-perf-exploit W1-1).
///
/// `None` (no `?quant=` override present) resolves to
/// [`DEFAULT_QUANTIZATION`] (`Sq8`) — the new Lunaris default now that
/// Moon's SQ8 path carries SIMD int8 ADC + exact f16 rerank. `Some(q)`
/// (an explicit override, e.g. `?quant=tq4`) is honored unchanged.
///
/// Pulled out as a pure function (mirrors [`parse_op_timeout`]) so the
/// default-resolution logic is unit-testable without a live Moon —
/// `MoonClient::connect_with_dim` cannot be exercised without network IO,
/// but the policy decision ("what does an absent `?quant=` mean") can be.
fn resolve_quantization(url_quant: Option<Quantization>) -> Quantization {
    url_quant.unwrap_or(DEFAULT_QUANTIZATION)
}

/// Parse the `?ef=` URL value into an `EF_RUNTIME` override
/// (moon-v051-perf-exploit W1-2).
///
/// - Absent (`None`) → `None`: **no forced default**. Moon's saturation
///   -gated adaptive ef (per-segment self-probe against the exact f16
///   rerank sidecar at compact time; see `vendor/moon/CHANGELOG.md`
///   [Unreleased] "Saturation-gated adaptive ef") already picks a
///   good-or-better ef per segment than any single fixed value we could
///   hardcode — forcing e.g. `64` would REGRESS segments the adaptive
///   pass would have kept at a higher beam width. This is a deliberate
///   project-lead call: expose the knob, don't second-guess Moon's own
///   heuristic by default.
/// - `Some(garbage)` or out of Moon's accepted `10..=4096` range
///   (`vendor/moon/src/command/vector_search/ft_create.rs`) → `warn!` and
///   fall back to `None` (never panics, never sends an invalid value to
///   the wire). Mirrors [`parse_op_timeout`]'s never-crash contract.
/// - `Some(valid)` → `Some(value)`, threaded into `FT.CREATE … EF_RUNTIME
///   <n>` at index-creation time. NOTE: as of the f9ad681f pin (post-v0.6.0)
///   Moon ALSO accepts `FT.CONFIG SET <idx> EF_RUNTIME <n>` at runtime
///   (`vendor/moon/src/command/vector_search/ft_config.rs`, 10..=4096, `0`
///   restores the auto heuristic) — landed in the mid-wave re-bump after
///   this module was first written against the older pin. Lunaris currently
///   applies `?ef=` only at creation; wiring the runtime path (so `?ef=`
///   also retunes PRE-EXISTING indices at connect) is a recorded follow-up
///   in docs/design/moon-v051-perf-exploit.md.
fn parse_ef_runtime(raw: Option<&str>) -> Option<u32> {
    const MIN: u32 = 10;
    const MAX: u32 = 4096;
    match raw {
        None => None,
        Some(raw) => match raw.trim().parse::<u32>() {
            Ok(n) if (MIN..=MAX).contains(&n) => Some(n),
            _ => {
                tracing::warn!(
                    value = %raw,
                    "ignoring invalid ?ef= (want a whole number {MIN}-{MAX}); \
                     leaving EF_RUNTIME unset (Moon's adaptive-ef default stays in effect)"
                );
                None
            }
        },
    }
}

/// Map a [`Quantization`] wire tier to the string Moon's `FT.INFO`
/// `QUANTIZATION` field reports for it. Deliberately NOT the same strings
/// as [`Quantization::as_wire`] — the `FT.CREATE … QUANTIZATION <q>` wire
/// arg (`TQ4`, `SQ8`, …) and the `FT.INFO` read-back value
/// (`vendor/moon/src/command/vector_search/helpers.rs::quantization_to_bytes`)
/// use different vocabularies (`TurboQuant4` vs `TQ4`) for the same tier.
/// Used only by the connect-time drift warning in
/// `assert_existing_index_dims_match`.
fn quantization_server_name(q: Quantization) -> &'static str {
    match q {
        Quantization::Tq1 => "TurboQuant1",
        Quantization::Tq2 => "TurboQuant2",
        Quantization::Tq3 => "TurboQuant3",
        Quantization::Tq4 => "TurboQuant4",
        Quantization::Sq8 => "SQ8",
    }
}

/// Default FT vector-index dimension used by the bare [`MoonClient::connect`] /
/// [`MoonStorage::connect`](crate::MoonStorage::connect) constructors. Matches
/// EmbeddingGemma-300M (the default Lunaris embedder). Callers wiring a wider
/// embedder (e.g. OpenAI `text-embedding-3` at 1536-d) should use
/// [`MoonClient::connect_with_dim`] instead — Moon's `FT.CREATE` has no
/// dimension cap (`DIM > 0` is the only constraint).
pub const DEFAULT_VECTOR_DIM: usize = 768;

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
    /// FT vector-index dimension this client creates indices at. Set at
    /// `connect`/`connect_with_dim` time; `> 0` invariant enforced by the
    /// constructor. Read by `ensure_indexes` (and re-exported to
    /// `MoonStorage::create_scope_indexes` via `self.client.dim`).
    pub dim: usize,
    /// Effective FT quantization tier this handle creates indices at —
    /// resolved by [`resolve_quantization`] from the `?quant=...` query
    /// param. Always `Some` since v0.5.1 (defaults to
    /// [`DEFAULT_QUANTIZATION`] when `?quant=` is absent); routes BOTH
    /// index-creation sites (legacy global `ensure_indexes` + per-scope
    /// `create_scope_indexes`) through the raw `FT.CREATE … QUANTIZATION
    /// <q>` path.
    pub quantization: Option<Quantization>,
    /// Optional `EF_RUNTIME` HNSW search-beam-width override from the
    /// `?ef=...` query param (moon-v051-perf-exploit W1-2). `None` (the
    /// default) leaves Moon's saturation-gated adaptive ef in charge —
    /// see [`parse_ef_runtime`] for the rationale against a hardcoded
    /// default. Sticky at `FT.CREATE` time, like `quantization`.
    pub ef_runtime: Option<u32>,
    /// The typed `moon-client` SDK handle. Cheap to clone.
    pub(crate) inner: TypedClient,
}

impl std::fmt::Debug for MoonClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoonClient")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("workspace", &self.workspace)
            .field("dim", &self.dim)
            .field("quantization", &self.quantization)
            .field("ef_runtime", &self.ef_runtime)
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
        Self::connect_with_dim(url, DEFAULT_VECTOR_DIM).await
    }

    /// Like [`MoonClient::connect`], but creates the FT vector indices at `dim`
    /// instead of the [`DEFAULT_VECTOR_DIM`] (768).
    ///
    /// `dim` MUST be `> 0`; `0` returns `StorageError::Backend("vector dim must
    /// be > 0")` before any network IO. Moon's `FT.CREATE` has no upper cap, so
    /// a 1536-d embedder (OpenAI `text-embedding-3`), 384-d, etc. all work.
    ///
    /// ## Operator footgun — existing index won't auto-resize
    ///
    /// Moon's `FT.CREATE` is idempotent and does NOT update an existing index's
    /// schema. If a Moon instance already holds a 768-d `chunks` index from a
    /// previous run and you reopen with a 1536-d embedder, `create_index`
    /// returns "already exists" and the dimension mismatch only surfaces later
    /// on the first vector write. Drop the stale index (`FT.DROPINDEX <name>`)
    /// before reopening at the new dimension.
    pub async fn connect_with_dim(url: &str, dim: usize) -> Result<Self, StorageError> {
        let parsed = url::Url::parse(url)
            .map_err(|e| StorageError::UnsupportedScheme(format!("moon parse: {e}")))?;
        if parsed.scheme() != "moon" {
            return Err(StorageError::UnsupportedScheme(parsed.scheme().into()));
        }
        if dim == 0 {
            return Err(StorageError::Backend("vector dim must be > 0".into()));
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| StorageError::Backend("moon URL missing host".into()))?
            .to_string();
        let port = parsed.port().unwrap_or(DEFAULT_MOON_PORT);
        let workspace = parsed.query_pairs().find(|(k, _)| k == "ws").map(|(_, v)| v.into_owned());
        // `?quant=<tier>` — parsed BEFORE any network IO so an invalid value
        // surfaces as the named `moon_invalid_quantization` code, never a
        // transport error. Order-independent with `?ws=`. An absent
        // `?quant=` resolves to `DEFAULT_QUANTIZATION` (Sq8) via
        // `resolve_quantization` — v0.5.1+ default flip (W1-1).
        let requested_quant = parsed
            .query_pairs()
            .find(|(k, _)| k == "quant")
            .map(|(_, v)| v.parse::<Quantization>())
            .transpose()?;
        let quantization = Some(resolve_quantization(requested_quant));
        // `?ef=<n>` — HNSW EF_RUNTIME override (W1-2). Never rejects the
        // connect attempt: an invalid value warns and falls back to `None`
        // (Moon's adaptive-ef default) via `parse_ef_runtime`.
        let ef_runtime = parsed.query_pairs().find(|(k, _)| k == "ef").map(|(_, v)| v.into_owned());
        let ef_runtime = parse_ef_runtime(ef_runtime.as_deref());

        // Moon speaks RESP2/RESP3 over the Redis protocol. We dial via the typed
        // moon-client SDK which internally opens a `redis::aio::MultiplexedConnection`.
        //
        // Two distinct bounds protect the ingest/recall handlers from a wedged Moon:
        //   * the 300s outer `tokio::time::timeout` bounds CONNECTION ESTABLISHMENT
        //     (TCP + handshake). Bulk-write bench workloads (50K-1M HSET ops) make a
        //     short connect bound unsafe, so this stays generous; and
        //   * `connect_with_timeout(.., moon_op_timeout())` sets a PER-COMMAND response
        //     timeout (`LUNARIS_MOON_OP_TIMEOUT`, default 10s) on the multiplexed
        //     connection, so EVERY later command (HSET / FT.* / TXN) is bounded. Without
        //     it a Moon that stalls mid-operation hangs the handler indefinitely
        //     (io-failsafe-wiring P0). The redis-rs ~500ms command default is not a
        //     contract and is far too aggressive for production batches, so we set ours
        //     explicitly. `connect_with_timeout` is in the vendored + published
        //     `moondb 0.2.1` path-dep this workspace builds against.
        let redis_url = format!("redis://{host}:{port}");
        let connect_timeout = std::time::Duration::from_secs(300);
        let inner = tokio::time::timeout(
            connect_timeout,
            TypedClient::connect_with_timeout(redis_url.as_str(), moon_op_timeout()),
        )
        .await
        .map_err(|_| {
            StorageError::Backend(format!(
                "moon connect timed out after {}s",
                connect_timeout.as_secs()
            ))
        })?
        .map_err(moon_err)?;
        let me = Self { host, port, workspace, dim, quantization, ef_runtime, inner };
        // Phase 22 dim-guardrail (+ W1-1 quantization-drift warning): probe
        // existing indices BEFORE `ensure_indexes` creates anything. If any
        // index already exists at a different dim, fail fast — Moon's
        // FT.CREATE is idempotent so ensure_indexes would otherwise
        // silently keep the old dim and the first vector write would
        // explode at runtime with a length mismatch. A quantization
        // mismatch is NOT fatal (graceful degrade — see
        // `assert_existing_index_dims_match`'s doc).
        me.assert_existing_index_dims_match().await?;
        me.ensure_indexes().await?;
        Ok(me)
    }

    /// Phase 22 — fail-fast dim guardrail for Moon.
    ///
    /// Probes `FT._LIST` + `FT.INFO` for each Lunaris-owned index
    /// (`chunks`, `entities`, `facts`, `communities`). If any pre-existing
    /// index reports a dim that disagrees with `self.dim`, return a
    /// `StorageError::Backend` with an actionable message naming the
    /// mismatched index. Missing indices are NOT an error — they get
    /// created at `self.dim` by `ensure_indexes` later in the same
    /// connect flow.
    ///
    /// We surface `list_indexes` / `index_info` transport errors as
    /// `StorageError::Backend` rather than silently letting `ensure_indexes`
    /// proceed: a partial-failure probe is worse than a hard error because
    /// the operator can't tell whether they're safe.
    ///
    /// ## W1-1 — quantization-drift warning (NOT fatal)
    ///
    /// Moon's `FT.INFO` now reports a `QUANTIZATION` field (server-internal
    /// names: `TurboQuant1..4` / `SQ8`; see `quantization_server_name`).
    /// Unlike the dim mismatch above, a quantization mismatch does NOT fail
    /// `connect` — `FT.CREATE`'s quantization is sticky exactly like dim,
    /// but re-quantizing in place is a lossy, expensive re-encode (Moon
    /// forbids `MERGE_MODE KEEP_RAW` for exactly this reason), so silently
    /// keeping the existing tier and warning is the safer default. Callers
    /// that need the new tier must `FT.DROPINDEX` + re-ingest, same as dim.
    async fn assert_existing_index_dims_match(&self) -> Result<(), StorageError> {
        let typed = self.inner.clone();
        // FT._LIST returns every index name currently on the Moon instance.
        // Cheap (single round-trip) and avoids per-index round-trips for
        // indices that don't exist.
        let existing = typed.vector().list_indexes().await.map_err(moon_err)?;
        let expected = ["chunks", "entities", "facts", "communities"];
        let wanted_quant = self.quantization.unwrap_or(DEFAULT_QUANTIZATION);
        let wanted_quant_server_name = quantization_server_name(wanted_quant);
        for name in expected.iter().filter(|n| existing.iter().any(|e| e == *n)) {
            let info = typed.vector().index_info(name).await.map_err(moon_err)?;
            let actual = info.dimension;
            // FT.INFO reports dimension as i64. A negative or zero value would
            // indicate a parse miss from `parse_index_info`; treat anything
            // <= 0 as "couldn't determine" and skip rather than spuriously
            // failing the open.
            if actual <= 0 {
                continue;
            }
            if (actual as usize) != self.dim {
                return Err(StorageError::Backend(format!(
                    "moon: vector dim mismatch on index {name:?} — existing index is {actual}-d, \
                     this Lunaris handle is configured for {wanted}-d. \
                     The Moon FT.CREATE schema is sticky and does NOT auto-resize. \
                     To proceed: (a) reopen the handle at the existing dim by passing an embedder \
                     that reports {actual}; OR (b) drop the stale indices \
                     (FT.DROPINDEX chunks entities facts communities) and re-ingest from source. \
                     There is no in-place rewrite path in v0.3 — see \
                     docs/migration/0.2-to-0.3-optional-embedder.md.",
                    wanted = self.dim,
                )));
            }
            if let Some(actual_quant) = info.extra.get("QUANTIZATION")
                && actual_quant != wanted_quant_server_name
            {
                tracing::warn!(
                    index = %name,
                    existing_quantization = %actual_quant,
                    configured_quantization = %wanted_quant_server_name,
                    "moon: pre-existing index quantization tier differs from this handle's \
                     configured tier — FT.CREATE quantization is sticky and does NOT \
                     auto-migrate; the EXISTING tier remains in effect for this index \
                     (connect proceeds — this is a graceful degrade, not a hard failure). \
                     Reopen after FT.DROPINDEX + re-ingest to change tiers."
                );
            }
        }
        Ok(())
    }

    /// Cheap clone of the underlying typed client. Use one clone per concurrent task.
    pub fn typed(&self) -> TypedClient {
        self.inner.clone()
    }

    /// Liveness `PING` round-trip on the established connection
    /// (`observability-rollout-maturity`). `Ok(())` when Moon answers `PONG`;
    /// `Err(StorageError::Backend)` when Moon is dead / stalled / unreachable.
    /// Bounded by `LUNARIS_MOON_OP_TIMEOUT` because the connection was opened
    /// with `connect_with_timeout(..)` (io-failsafe Half B), so the `/healthz`
    /// probe can never hang the handler.
    pub async fn ping(&self) -> Result<(), StorageError> {
        let mut typed = self.typed();
        redis::cmd("PING")
            .query_async::<String>(typed.inner_mut())
            .await
            .map(|_| ())
            .map_err(redis_err)
    }

    /// Idempotently create the FT indexes Lunaris uses (`chunks`, `entities`,
    /// `facts`, `communities`). Each index declares the dense `vec` field
    /// (HNSW Cosine, `self.dim`-d — set at `connect`/`connect_with_dim`;
    /// defaults to [`DEFAULT_VECTOR_DIM`]) AND a TEXT `content` field so BM25 / HYBRID
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
        let dim = self.dim;
        for (name, prefix) in &[
            ("chunks", "chunks:"),
            ("entities", "entities:"),
            ("facts", "facts:"),
            ("communities", "communities:"),
        ] {
            create_lunaris_index(
                &self.inner,
                name,
                prefix,
                dim,
                self.quantization,
                self.ef_runtime,
            )
            .await?;
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

/// Idempotently create one Lunaris FT index, honoring the per-handle
/// quantization choice. Shared by `MoonClient::ensure_indexes` (legacy global
/// indices) and `MoonStorage::create_scope_indexes` (per-scope indices) so the
/// two creation sites can never diverge on schema or quantization.
///
/// `kind` is the logical index kind (`"chunks"` / `"entities"` / `"facts"` /
/// `"communities"`) — it controls the extra schema fields; `name` is the
/// actual FT index name (equal to `kind` for the legacy global indices,
/// `ft_index_name(scope, kind)` for per-scope ones), and `prefix` the HSET
/// key prefix the index auto-covers.
///
/// - `quant: None` → the typed-SDK path, byte-identical to pre-quantization
///   behavior (server default TQ4). No longer reachable through the public
///   `moon://` URL grammar since [`resolve_quantization`] always resolves an
///   absent `?quant=` to `Some(DEFAULT_QUANTIZATION)` (W1-1) — kept for
///   defensive completeness and so `Quantization`'s `None` state (a valid
///   value of the type) still has a defined, tested behavior.
/// - `quant: Some(q)` → raw RESP `FT.CREATE` mirroring the SDK's exact HNSW
///   arg layout (vendor/moon/sdk/rust/src/vector.rs::create_index — base
///   5 pairs = param_count 10) with `QUANTIZATION <q>` appended
///   (param_count 12, +2 more if `ef` is also set). Moon's typed
///   `VectorIndexOptions` has no quantization slot, hence the raw escape
///   hatch — see `ensure_indexes_uses_per_client_dim_not_const` and sibling
///   structural tests that pin this file's shape.
///
/// `ef: Some(n)` sets `EF_RUNTIME <n>` at creation time in BOTH arms (W1-2)
/// — Moon has no `FT.CONFIG SET EF_RUNTIME` (verified against
/// `vendor/moon/src/command/vector_search/ft_config.rs`), so this is the
/// only way to pin the HNSW search beam width; like quantization, it is
/// sticky and does not migrate an existing index.
///
/// "already exists" replies are swallowed in both arms — Moon's `FT.CREATE`
/// is idempotent and sticky-schema (an existing index keeps its dim AND its
/// quantization/ef; see the dim footgun on `connect_with_dim`).
pub(crate) async fn create_lunaris_index(
    typed: &TypedClient,
    kind: &str,
    prefix: &str,
    dim: usize,
    quant: Option<Quantization>,
    ef: Option<u32>,
) -> Result<(), StorageError> {
    create_lunaris_index_named_ef(typed, kind, kind, prefix, dim, quant, ef).await
}

/// Full implementation behind [`create_lunaris_index`] (legacy global site)
/// and `lib.rs::create_scope_indexes` (per-scope site) — decouples the FT
/// index `name` from the schema-selecting `kind`. Both creation sites thread
/// the handle's `?quant=`/`?ef=` choices through here so they can never
/// diverge.
///
/// `pub` (not `pub(crate)`, unlike its siblings): the moon-v051-perf-exploit
/// W1-2 live test suite (`tests/a_quant_ef_guardrails.rs`) calls this
/// directly against a uniquely-named per-test index rather than through
/// `MoonClient::connect`'s legacy global `chunks`/`entities`/`facts`/
/// `communities` names — those are process-lifetime-sticky and shared
/// across EVERY live test in this crate that connects at all, making them
/// unsuitable for a deterministic "does `?ef=` reach a fresh index" test
/// once more than one such test exists in the same Moon server lifetime.
#[allow(clippy::too_many_arguments)]
pub async fn create_lunaris_index_named_ef(
    typed: &TypedClient,
    name: &str,
    kind: &str,
    prefix: &str,
    dim: usize,
    quant: Option<Quantization>,
    ef: Option<u32>,
) -> Result<(), StorageError> {
    use moon::{DistanceMetric, SchemaField, VectorIndexOptions};
    // Every index carries a TEXT `content` field so BM25 / HYBRID FT.SEARCH
    // can score the per-row text payload (Gap 9). Plan 09.1-02 — chunks adds
    // `valid_time` NUMERIC so `@valid_time:[lo hi]` hits a real indexed
    // field; Plan 15-01 — chunks adds `source` TAG so `@source:{value}`
    // resolves server-side (PERF-MOON-01).
    let mut extra = vec![SchemaField::Text("content".to_string())];
    if kind == "chunks" {
        extra.push(SchemaField::Numeric("valid_time".to_string()));
        extra.push(SchemaField::Tag("source".to_string()));
    }

    match quant {
        None => {
            let mut opts = VectorIndexOptions::new(dim, DistanceMetric::Cosine)
                .prefix(prefix)
                .field_name("vec");
            if let Some(e) = ef {
                opts = opts.ef_runtime(e as usize);
            }
            for field in extra {
                opts = opts.add_field(field);
            }
            let typed = typed.clone();
            match typed.vector().create_index(name, opts).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("Index already exists") || msg.contains("already exists") {
                        Ok(())
                    } else {
                        Err(moon_err(e))
                    }
                }
            }
        }
        Some(q) => {
            let mut cmd = redis::cmd("FT.CREATE");
            cmd.arg(name).arg("ON").arg("HASH").arg("PREFIX").arg("1").arg(prefix).arg("SCHEMA");
            for field in &extra {
                for arg in field.to_args() {
                    cmd.arg(arg);
                }
            }
            // SDK base layout: TYPE, DIM, DISTANCE_METRIC, M, EF_CONSTRUCTION
            // = 5 pairs = 10 args; QUANTIZATION adds one pair → 12;
            // EF_RUNTIME (when set) adds one more pair → 14.
            let param_count: usize = 12 + if ef.is_some() { 2 } else { 0 };
            cmd.arg("vec")
                .arg("VECTOR")
                .arg("HNSW")
                .arg(param_count)
                .arg("TYPE")
                .arg("FLOAT32")
                .arg("DIM")
                .arg(dim)
                .arg("DISTANCE_METRIC")
                .arg("COSINE")
                .arg("M")
                .arg(16usize)
                .arg("EF_CONSTRUCTION")
                .arg(200usize)
                .arg("QUANTIZATION")
                .arg(q.as_wire());
            if let Some(e) = ef {
                cmd.arg("EF_RUNTIME").arg(e);
            }
            let mut typed = typed.clone();
            match cmd.query_async::<String>(typed.inner_mut()).await {
                Ok(_) => Ok(()),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("Index already exists") || msg.contains("already exists") {
                        Ok(())
                    } else {
                        Err(redis_err(e))
                    }
                }
            }
        }
    }
}

/// Resolve the per-command Moon response timeout from `LUNARIS_MOON_OP_TIMEOUT`
/// (whole seconds). Unset → the 10s default silently; unparseable or `≤ 0` → the
/// 10s default with a `warn!`. Never returns a zero or "infinite" timeout.
///
/// This is the value handed to [`TypedClient::connect_with_timeout`] in
/// [`MoonClient::connect_with_dim`], so every Moon command on the resulting
/// connection (HSET / FT.* / TXN) is response-bounded — a stalled Moon can no
/// longer hang the ingest/recall handler (io-failsafe-wiring P0).
fn moon_op_timeout() -> std::time::Duration {
    parse_op_timeout(std::env::var("LUNARIS_MOON_OP_TIMEOUT").ok().as_deref())
}

/// Pure parser for the `LUNARIS_MOON_OP_TIMEOUT` value (whole seconds). `None`
/// (unset) → the 10s default, silently; a `Some` that is unparseable or `≤ 0` →
/// the 10s default with a `warn!`. Never returns a zero or "infinite" timeout.
///
/// Split out from [`moon_op_timeout`] so the edge cases are unit-testable without
/// mutating the process environment — this crate is `#![forbid(unsafe_code)]` and
/// `std::env::set_var` is `unsafe` on edition 2024.
fn parse_op_timeout(raw: Option<&str>) -> std::time::Duration {
    const DEFAULT_SECS: u64 = 10;
    match raw {
        // Unset is the normal case — the default, no warning.
        None => std::time::Duration::from_secs(DEFAULT_SECS),
        // Garbage ("abc"), non-positive ("0", "-1"), or overflow: never crash,
        // never an infinite/zero timeout — fall back to the default with a warn.
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => std::time::Duration::from_secs(secs),
            _ => {
                tracing::warn!(
                    value = %raw,
                    "ignoring invalid LUNARIS_MOON_OP_TIMEOUT (want a positive whole number \
                     of seconds); using {DEFAULT_SECS}s default"
                );
                std::time::Duration::from_secs(DEFAULT_SECS)
            }
        },
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

    /// io-failsafe-wiring (Half B): `moon_op_timeout()` resolves the per-op Moon
    /// response timeout from `LUNARIS_MOON_OP_TIMEOUT` (whole seconds). Unset →
    /// 10s default; a valid positive value is honored; garbage or ≤0 falls back
    /// to the 10s default (warn, never crash, never an infinite/zero timeout).
    /// We exercise the pure parser directly — the crate is
    /// `#![forbid(unsafe_code)]`, so mutating the process env (unsafe on edition
    /// 2024) is not an option, and the env-reading wrapper `moon_op_timeout()` is
    /// covered end-to-end by `tests/op_timeout.rs`.
    #[test]
    fn moon_op_timeout_default_and_override() {
        use std::time::Duration;
        assert_eq!(parse_op_timeout(None), Duration::from_secs(10), "unset -> 10s default");
        assert_eq!(parse_op_timeout(Some("5")), Duration::from_secs(5), "\"5\" -> 5s");
        assert_eq!(
            parse_op_timeout(Some("abc")),
            Duration::from_secs(10),
            "garbage -> 10s default"
        );
        assert_eq!(parse_op_timeout(Some("0")), Duration::from_secs(10), "0 -> 10s default");
    }

    // ── W1-1: SQ8-default resolution (moon-v051-perf-exploit) ──

    /// RED (pre-fix): the old default resolved an absent `?quant=` to
    /// `None` (server default TQ4 via the typed-SDK path). GREEN: it now
    /// resolves to `Sq8` — Moon's SIMD int8 ADC + exact f16 rerank beats
    /// TQ4 on both recall and per-candidate throughput at 768-d.
    #[test]
    fn resolve_quantization_defaults_to_sq8_when_url_omits_quant() {
        assert_eq!(resolve_quantization(None), Quantization::Sq8);
    }

    #[test]
    fn resolve_quantization_honors_explicit_tq4_override() {
        assert_eq!(resolve_quantization(Some(Quantization::Tq4)), Quantization::Tq4);
    }

    #[test]
    fn resolve_quantization_honors_explicit_sq8_override() {
        assert_eq!(resolve_quantization(Some(Quantization::Sq8)), Quantization::Sq8);
    }

    #[test]
    fn quantization_server_name_maps_wire_tier_to_ft_info_vocabulary() {
        assert_eq!(quantization_server_name(Quantization::Sq8), "SQ8");
        assert_eq!(quantization_server_name(Quantization::Tq4), "TurboQuant4");
        assert_eq!(quantization_server_name(Quantization::Tq1), "TurboQuant1");
    }

    // ── W1-2: `?ef=` EF_RUNTIME parsing (moon-v051-perf-exploit) ──

    #[test]
    fn parse_ef_runtime_unset_leaves_moons_adaptive_ef_in_charge() {
        assert_eq!(parse_ef_runtime(None), None, "no forced default — see doc rationale");
    }

    #[test]
    fn parse_ef_runtime_accepts_value_in_range() {
        assert_eq!(parse_ef_runtime(Some("64")), Some(64));
        assert_eq!(parse_ef_runtime(Some("10")), Some(10), "lower bound inclusive");
        assert_eq!(parse_ef_runtime(Some("4096")), Some(4096), "upper bound inclusive");
    }

    #[test]
    fn parse_ef_runtime_rejects_below_min_falls_back_to_none() {
        assert_eq!(parse_ef_runtime(Some("9")), None);
        assert_eq!(parse_ef_runtime(Some("0")), None);
    }

    #[test]
    fn parse_ef_runtime_rejects_above_max_falls_back_to_none() {
        assert_eq!(parse_ef_runtime(Some("4097")), None);
    }

    #[test]
    fn parse_ef_runtime_rejects_garbage_falls_back_to_none() {
        assert_eq!(parse_ef_runtime(Some("abc")), None);
        assert_eq!(parse_ef_runtime(Some("-5")), None);
        assert_eq!(parse_ef_runtime(Some("")), None);
    }

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

    /// `connect_with_dim(url, 0)` MUST reject before any network IO — the
    /// `dim > 0` invariant is checked right after URL parse / scheme check,
    /// so this test needs no live Moon. (We pass a syntactically valid
    /// `moon://` URL so the scheme check passes and we reach the dim guard.)
    #[tokio::test]
    async fn connect_with_dim_rejects_zero_dim() {
        let r = MoonClient::connect_with_dim("moon://localhost:6380", 0).await;
        match r {
            Err(StorageError::Backend(msg)) => {
                assert!(msg.contains("vector dim must be > 0"), "unexpected msg: {msg}");
            }
            other => panic!("expected Backend(vector dim must be > 0), got {other:?}"),
        }
    }

    /// Structural guard: `ensure_indexes` builds `VectorIndexOptions` from the
    /// per-client `dim` field, not a hard-coded constant. A 1536-d embedder
    /// must produce a 1536-d `vec` field (validated end-to-end by the
    /// `moon-it`-gated `dim_configurable` integration test).
    #[test]
    fn ensure_indexes_uses_per_client_dim_not_const() {
        let source = include_str!("client.rs");
        assert!(
            source.contains("VectorIndexOptions::new(dim, DistanceMetric::Cosine)"),
            "ensure_indexes must size the FT index from the stored `dim`, not a const"
        );
        // The old hard-coded constant for the FT vector dimension must be gone
        // from this file. We assemble the needle from pieces so this assertion
        // does not match itself in the `include_str!`'d source.
        let needle = format!("const {} usize = {}", "DIM:", 768);
        assert!(
            !source.contains(&needle),
            "the hard-coded const-DIM declaration must be gone from ensure_indexes"
        );
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

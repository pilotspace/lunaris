//! Plan 04-05 — `Lunaris::forget(target) -> ForgetReceipt`.
//!
//! Single-entry-point ergonomic API closing OPS-01 (id), OPS-02 (scope),
//! OPS-03 (before), OPS-04 (audit on every call). Three-variant builder with
//! `.hard()` / `.dry_run()` flags + `ForgetConfirmation` two-step token for
//! the D-21 hard-delete safety rail.
//!
//! ## Fixes baked in
//!
//! - **B-3**: hard-delete without token returns
//!   `LunarisError::Validate(ValidateError::ConfirmationRequired(_))` — uses
//!   the typed variant added in Task 0; NOT the non-existent
//!   `LunarisError::Validate(String)` shape.
//! - **B-4**: every HLC stamp comes from `clock.tick()` (HlcClock method);
//!   `Hlc::now()` does not exist in the codebase.
//! - **B-5**: soft-delete mutates the typed `BiTemporal::invalidate_sys(now)`
//!   helper. The mutated `bt` is then JSON-patched into `payload["bt"]["sys"][1]`
//!   so backends that derive persisted bt from the payload bytes (Moon HSET
//!   layer + Postgres row storage) see the change. Mirrors Plan 04-04 Task 4
//!   `apply_supersede` verbatim.
//! - **W-1**: `match_before` deserializes `bt.valid.0` into a typed `Hlc` and
//!   uses the `<` operator (Hlc derives Ord). NOT a `format!("{hlc:?}")` lex
//!   compare.
//! - **D-19 single atomic_write invariant**: every successful forget call
//!   issues at most ONE `storage.atomic_write` (zero for dry_run / no-match).
//! - **D-22 audit emit**: every successful forget call ends with one
//!   `audit::publish_audit_event(storage, AuditEvent::Forget(receipt))`.

use lunaris_core::storage::types::{Lsn, WriteOp};
use lunaris_core::{
    BiTemporal, Hlc, HlcClock, LunarisError, StorageError, StoragePort, ValidateError,
};
use serde::{Deserialize, Serialize};
// Plan 05-05 OPS-05 — `Instrument::instrument` wraps the per-call body in the
// `lunaris.forget` info_span so per-call `correlation_id` field-recording
// + downstream child-span propagation works (CONTEXT.md D-24).
use tracing::Instrument;
use ulid::Ulid;

use crate::audit::{AuditEvent, publish_audit_event};
use crate::handle::Lunaris;

// ---------------------------------------------------------------------------
// Public DTOs (locked by Task 1 stub; Task 2 only adds the impl block + helpers)
// ---------------------------------------------------------------------------

/// Single-entry-point target for `Lunaris::forget` per D-18. Three variants
/// closing OPS-01 / OPS-02 / OPS-03.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ForgetTarget {
    /// OPS-01: single-target purge across KV + vector + graph indices.
    Id(Ulid),
    /// OPS-02: scope purge — soft-delete by default; `.hard()` requires
    /// confirmation token (D-21 safety rail).
    Scope(ScopeSpec),
    /// OPS-03: temporal-bound purge using AS_OF semantics (D-19).
    Before(Hlc),
}

/// Match language for `ForgetTarget::Scope` per D-20. v0 supports prefix-match
/// on `source` (the helios:fs/ session-pruning case), exact metadata kv match,
/// and exact episode-id match. Richer predicate languages are v1.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ScopeSpec {
    /// Prefix match on the JSON `source` field (e.g., `"helios:fs/session-42/"`).
    BySource(String),
    /// Exact match on a single `metadata.<key> == <value>` pair.
    ByMetadata(String, String),
    /// Exact match on the JSON `id` field.
    ByEpisode(Ulid),
}

/// Tag for which storage indices a forget call touched. Mirrors the
/// `lunaris_consolidate::types::IndexKind` shape so Plan 04-05 can wire them
/// 1:1 when the audit emit surface is unified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum IndexKind {
    Kv,
    Vector,
    Graph,
}

/// Receipt returned by every `Lunaris::forget` call. Carries enough
/// information for the caller to reconstruct what was attempted (preview)
/// vs what was committed (rows_written / rows_deleted).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetReceipt {
    pub target: ForgetTarget,
    pub indices_affected: Vec<IndexKind>,
    /// **Episodes** the target matched — the blast radius a committing call
    /// would have, counted in the unit the caller reasons in.
    ///
    /// Deliberately NOT the row count. W1.4 made forget sweep each matched
    /// episode's chunk rows, so `rows_written` / `rows_deleted` now report 2+
    /// where they used to report 1 — but "I am about to delete 2 things" is a
    /// worse answer to `memory.forget --dry-run` than "1 episode", and the
    /// MCP-facing DTO in `lunaris-memory-service` documents this as episodes.
    /// Rows are an implementation detail of how an episode is stored; the
    /// episode is what the caller asked to forget.
    ///
    /// Populated on EVERY path, including `dry_run`, where `rows_written` and
    /// `rows_deleted` are both zero by construction: a preview whose only
    /// counters are zeroes tells the caller nothing about the blast radius it
    /// is being asked to confirm (0.6.2 Task F — the MCP `memory.forget`
    /// preview surfaces this as `matched`).
    ///
    /// `#[serde(default)]` keeps receipts minted by pre-0.6.2 servers
    /// deserializable — `POST /v1/forget` carries a serialized prior receipt
    /// in `confirmation_token`, so an old client's token must still parse.
    #[serde(default)]
    pub matched: u64,
    /// Soft-delete MVCC writes (zero for hard / dry-run).
    pub rows_written: u64,
    /// Irreversible deletes (hard-only; zero for soft / dry-run).
    pub rows_deleted: u64,
    /// `__lunaris_audit__` publish offset.
    pub audit_lsn: Lsn,
    /// `true` iff this was a `.dry_run()` preview that did NOT call
    /// `atomic_write`.
    pub preview: bool,
}

/// Opaque confirmation token returned by `Lunaris::confirm_hard_forget(dry_run)`
/// per D-21 hard-delete safety rail. Cannot be constructed by the caller —
/// only returned from the two-step protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetConfirmation {
    pub(crate) for_audit_lsn: Lsn,
}

// ---------------------------------------------------------------------------
// Builder (added in Task 2)
// ---------------------------------------------------------------------------

/// Internal options carried by `ForgetRequest`. Public so callers can match
/// on the receipt's preview field without reaching into private state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetOptions {
    pub hard: bool,
    pub dry_run: bool,
    pub confirmation_token: Option<ForgetConfirmation>,
}

/// Combined target + options. The `From<ForgetTarget>` impl below means
/// callers can pass a bare `ForgetTarget` to `Lunaris::forget(...)` for the
/// soft-delete default case AND chain `.hard()` / `.dry_run()` on the target
/// to get a full request.
///
/// Plan 08-02 Rule 3 additive: `Serialize + Deserialize` added so the codegen
/// PyO3 wrapper for `Lunaris::forget` can accept a Python dict via
/// `pythonize::depythonize`. Both inner fields (`ForgetTarget`, `ForgetOptions`)
/// already derived these traits, so the change is mechanical with no
/// semantic surface shift.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgetRequest {
    pub target: ForgetTarget,
    pub options: ForgetOptions,
}

impl ForgetTarget {
    /// Mark the request as a hard (irreversible) delete. The user MUST also
    /// call `.with_token(token)` from a prior `confirm_hard_forget(dry_run)`
    /// or the call returns
    /// `LunarisError::Validate(ValidateError::ConfirmationRequired(_))` per
    /// the D-21 safety rail (B-3 typed variant).
    pub fn hard(self) -> ForgetRequest {
        ForgetRequest { target: self, options: ForgetOptions { hard: true, ..Default::default() } }
    }

    /// Build a preview request that does NOT call `atomic_write`. Returns a
    /// `ForgetReceipt { preview: true, rows_written: 0, ... }` so the caller
    /// can inspect what would have been touched + (for hard delete) feed the
    /// receipt to `confirm_hard_forget` to receive a token.
    pub fn dry_run(self) -> ForgetRequest {
        ForgetRequest {
            target: self,
            options: ForgetOptions { dry_run: true, ..Default::default() },
        }
    }

    /// Wrap as a soft-delete request (default options).
    pub fn into_request(self) -> ForgetRequest {
        ForgetRequest { target: self, options: ForgetOptions::default() }
    }
}

impl ForgetRequest {
    /// Attach a confirmation token from a prior `confirm_hard_forget` call.
    /// Required for `.hard()` requests per the D-21 safety rail.
    pub fn with_token(mut self, token: ForgetConfirmation) -> Self {
        self.options.confirmation_token = Some(token);
        self
    }

    /// Convenience constructor for fully-confirmed hard delete in one expression.
    pub fn hard_confirmed(target: ForgetTarget, token: ForgetConfirmation) -> Self {
        ForgetRequest {
            target,
            options: ForgetOptions { hard: true, dry_run: false, confirmation_token: Some(token) },
        }
    }
}

impl From<ForgetTarget> for ForgetRequest {
    fn from(target: ForgetTarget) -> Self {
        target.into_request()
    }
}

// ---------------------------------------------------------------------------
// Lunaris impl block — `forget` + `confirm_hard_forget`
// ---------------------------------------------------------------------------

impl Lunaris {
    /// `Lunaris::forget(target) -> Result<ForgetReceipt, LunarisError>` —
    /// **DEPRECATED** as of P0 #1 Wave 2. Use
    /// `engine.scoped(scope).forget(request)` instead so the forget call
    /// inherits the bound partition key. The legacy path routes its
    /// internal storage calls through `Scope::dev()` and silently returns
    /// `rows_deleted = 0, rows_written = 0` under any non-`_dev_` scope.
    /// See `docs/v0.3-known-debt.md` for the removal plan (v0.4 task).
    ///
    /// ## Behaviour matrix (unchanged from v0.2)
    ///
    /// | Request                              | Result                                  |
    /// |--------------------------------------|-----------------------------------------|
    /// | `target` (soft)                      | One `atomic_write` of MVCC sys_to writes |
    /// | `target.dry_run()`                   | No `atomic_write`; preview receipt      |
    /// | `target.hard()` (no token)           | `Err(Validate(ConfirmationRequired))`   |
    /// | `target.hard().with_token(t)`        | One `atomic_write` of `KvDelete` ops    |
    // scope-dev-allowed: deprecation-note — message text references the
    // marker for adopters reading the rustc warning.
    #[deprecated(
        since = "0.3.0",
        note = "use `engine.scoped(scope).forget(request)` — Lunaris::forget routes through Scope::dev() and returns zero matches under any non-_dev_ scope"
    )]
    pub async fn forget(
        &self,
        request: impl Into<ForgetRequest>,
    ) -> Result<ForgetReceipt, LunarisError> {
        let request = request.into();

        // Plan 05-05 OPS-05 — `lunaris.forget` root span (CONTEXT.md D-24).
        // `correlation_id` reserved as `tracing::field::Empty` for HTTP-layer
        // propagation. `target` carries only the variant tag (`id` / `scope`
        // / `before`) — the actual ulid / scope spec is NOT logged
        // (T-05-05-04 accept disposition; richer redaction lands in v2
        // OPS-V2-01).
        let target_kind = match &request.target {
            ForgetTarget::Id(_) => "id",
            ForgetTarget::Scope(_) => "scope",
            ForgetTarget::Before(_) => "before",
        };
        let span = tracing::info_span!(
            "lunaris.forget",
            correlation_id = tracing::field::Empty,
            target = %target_kind,
            hard = %request.options.hard,
            dry_run = %request.options.dry_run,
        );
        async move {
            // P-2 (v0.2 review) — operator-visible warning that this path is
            // hard-coded to `Scope::dev()` for `atomic_write`, `read_as_of`,
            // and `scan_range`. Under any non-`_dev_` scope the call silently
            // returns `rows_deleted = 0, rows_written = 0` because RLS
            // (Postgres) or the SCAN prefix (Moon) filters everything out.
            // The full per-scope routing lands in v0.3 as
            // `ScopedLunaris::forget(target)`. Emitting this every call (not
            // once-per-process) so operators chasing "why didn't my forget
            // work" see the warning in the line right above their forget
            // invocation. Suppressible via the standard tracing filter
            // (`LUNARIS_LOG=lunaris::forget=error` or similar).
            // scope-dev-allowed: warn-string-mentions-marker — operator
            // readability text; ScopedLunaris::forget is the canonical fix.
            tracing::warn!(
                target: "lunaris::forget",
                "Lunaris::forget is hard-coded to Scope::dev() in v0.2.x — \
                 same-scope forget under any non-`_dev_` scope silently \
                 returns zero matches. ScopedLunaris::forget(target) lands \
                 in v0.3. See CHANGELOG.md v0.2.0 Known issues."
            );

            // B-3: hard delete must carry a confirmation token. Returns the typed
            // ValidateError::ConfirmationRequired variant added in Task 0.
            if request.options.hard && request.options.confirmation_token.is_none() {
                return Err(LunarisError::Validate(ValidateError::ConfirmationRequired(
                    "hard-delete requires confirmation token from prior dry_run".into(),
                )));
            }

            // B-4: scan_matches takes &HlcClock so it can call clock.tick() to
            // stamp the as_of for the OPS-01 single-target read_as_of path.
            let matches =
                scan_matches(self.storage.as_ref(), &request.target, self.clock.as_ref()).await?;

            if request.options.dry_run {
                // Dry-run: no atomic_write. Receipt carries preview=true.
                let receipt = ForgetReceipt {
                    target: request.target.clone(),
                    indices_affected: classify_indices(&request.target),
                    matched: matches.len() as u64,
                    rows_written: 0,
                    rows_deleted: 0,
                    audit_lsn: Lsn { wall_ms: 0, counter: 0 },
                    preview: true,
                };
                let audit_offset =
                    publish_audit_event(&self.storage, AuditEvent::Forget((&receipt).into()))
                        .await
                        .unwrap_or(0);
                return Ok(ForgetReceipt {
                    audit_lsn: Lsn { wall_ms: audit_offset, counter: 0 },
                    ..receipt
                });
            }

            // Build the WriteOp set: hard → KvDelete; soft → KvPut with MVCC
            // sys_to-stamped payload (B-4 + B-5).
            let ops: Vec<WriteOp> = if request.options.hard {
                matches.iter().map(|m| WriteOp::KvDelete { key: m.key.clone() }).collect()
            } else {
                matches
                    .iter()
                    .map(|m| build_soft_delete_op(m, self.clock.as_ref()))
                    .collect::<Result<Vec<_>, _>>()?
            };

            // D-19 single-call invariant: at most ONE atomic_write per forget
            // call. Skip when there are zero matches (publishing an empty op
            // batch would be a no-op anyway and some backends reject it).
            if !ops.is_empty() {
                // RFC 0001 Wave 0: use Scope::dev() until per-scope routing (Wave 1D).
                // scope-dev-allowed: deprecated-Lunaris::forget-routes-here-until-v0.4
                let _lsn = self
                    .storage
                    .atomic_write(&lunaris_core::Scope::dev(), &ops)
                    .await
                    .map_err(LunarisError::Storage)?;
            }

            let receipt = ForgetReceipt {
                target: request.target.clone(),
                indices_affected: classify_indices(&request.target),
                matched: matches.len() as u64,
                rows_written: if request.options.hard { 0 } else { matches.len() as u64 },
                rows_deleted: if request.options.hard { matches.len() as u64 } else { 0 },
                audit_lsn: Lsn { wall_ms: 0, counter: 0 },
                preview: false,
            };
            let audit_offset =
                publish_audit_event(&self.storage, AuditEvent::Forget((&receipt).into()))
                    .await
                    .unwrap_or(0);
            Ok(ForgetReceipt { audit_lsn: Lsn { wall_ms: audit_offset, counter: 0 }, ..receipt })
        }
        .instrument(span)
        .await
    }

    /// Two-step hard-delete safety rail per D-21. Caller MUST first run
    /// `forget(target.dry_run())` to receive a preview receipt, then pass that
    /// receipt to this method to obtain a `ForgetConfirmation` token. The
    /// token then unlocks `forget(target.hard().with_token(token))`.
    ///
    /// Returns `Err(Validate(ConfirmationRequired))` when called with a
    /// non-preview receipt (B-3 typed variant).
    pub async fn confirm_hard_forget(
        &self,
        dry_run_receipt: ForgetReceipt,
    ) -> Result<ForgetConfirmation, LunarisError> {
        if !dry_run_receipt.preview {
            return Err(LunarisError::Validate(ValidateError::ConfirmationRequired(
                "confirm_hard_forget requires a dry_run receipt (preview=true)".into(),
            )));
        }
        Ok(ForgetConfirmation { for_audit_lsn: dry_run_receipt.audit_lsn })
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// One match yielded by [`scan_matches`]. Carries the original BiTemporal so
/// [`build_soft_delete_op`] can mutate the typed struct via `invalidate_sys`
/// (B-5) instead of pure JSON manipulation.
#[derive(Clone, Debug)]
struct ForgetMatch {
    key: Vec<u8>,
    payload: Vec<u8>,
    bt: BiTemporal,
}

/// Walk the storage layer to find every primitive matching `target`. The
/// OPS-01 single-target path uses `read_as_of` directly; OPS-02 (Scope) and
/// OPS-03 (Before) walk `scan_range` and apply the typed predicate at the
/// Lunaris boundary.
///
/// B-4: takes `&HlcClock` so each row read can stamp a fresh `now` via
/// `clock.tick()` — preserves HLC monotony across the per-row read loop.
async fn scan_matches(
    storage: &dyn StoragePort,
    target: &ForgetTarget,
    clock: &HlcClock,
) -> Result<Vec<ForgetMatch>, LunarisError> {
    use futures::stream::StreamExt;

    // OPS-01 fast path: single-target read_as_of. Avoid the full prefix scan.
    if let ForgetTarget::Id(ulid) = target {
        let key_str = format!("episode:{ulid}");
        // B-4: clock.tick() — Hlc::now() doesn't exist in this codebase.
        let now = clock.tick();
        // scope-dev-allowed: deprecated-Lunaris::forget-routes-here-until-v0.4
        let row = storage
            .read_as_of(&lunaris_core::Scope::dev(), key_str.as_bytes(), now)
            .await
            .map_err(LunarisError::Storage)?;
        return Ok(match row {
            Some(r) => {
                vec![ForgetMatch { key: key_str.into_bytes(), payload: r.value.to_vec(), bt: r.bt }]
            }
            None => Vec::new(),
        });
    }

    // OPS-02 / OPS-03 path: prefix scan + typed predicate. v0 only walks the
    // `episode:` prefix; richer scope languages (chunk:, fact:, ...) are v1.
    let prefix: &[u8] = b"episode:";

    // RFC 0001 Wave 0: use Scope::dev() until per-scope routing (Wave 1D).
    // scope-dev-allowed: deprecated-Lunaris::forget-routes-here-until-v0.4
    let mut stream = storage
        .scan_range(&lunaris_core::Scope::dev(), prefix, None)
        .await
        .map_err(LunarisError::Storage)?;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let (k, v) = item.map_err(LunarisError::Storage)?;
        if matches_target(&k, &v, target) {
            // B-5: load the typed BiTemporal via read_as_of so build_soft_delete_op
            // can mutate the typed struct via `invalidate_sys`. now is bumped
            // per row to preserve HLC monotony.
            let now = clock.tick();
            // scope-dev-allowed: deprecated-Lunaris::forget-routes-here-until-v0.4
            let row_opt = storage
                .read_as_of(&lunaris_core::Scope::dev(), &k, now)
                .await
                .map_err(LunarisError::Storage)?;
            let bt = row_opt
                .as_ref()
                .map(|r| r.bt)
                .unwrap_or_else(|| BiTemporal { valid: (Hlc::ZERO, None), sys: (Hlc::ZERO, None) });
            out.push(ForgetMatch { key: k.to_vec(), payload: v.to_vec(), bt });
        }
    }
    Ok(out)
}

/// Predicate dispatch over the three target variants. The OPS-01 case is
/// already short-circuited in [`scan_matches`] above so this only sees Scope
/// + Before during the prefix-scan loop.
fn matches_target(_key: &[u8], value: &[u8], target: &ForgetTarget) -> bool {
    match target {
        ForgetTarget::Id(_) => true,
        ForgetTarget::Scope(spec) => match_scope(value, spec),
        ForgetTarget::Before(hlc) => match_before(value, *hlc),
    }
}

/// D-20 scope-match languages. v0 supports BySource (prefix), ByMetadata
/// (exact key/value), ByEpisode (exact id). Future versions may add
/// substring / regex / typed-filter trees.
fn match_scope(value: &[u8], spec: &ScopeSpec) -> bool {
    let json: serde_json::Value = match serde_json::from_slice(value) {
        Ok(j) => j,
        Err(_) => return false,
    };
    match spec {
        ScopeSpec::BySource(prefix) => json
            .get("source")
            .and_then(|s| s.as_str())
            .map(|s| s.starts_with(prefix))
            .unwrap_or(false),
        ScopeSpec::ByMetadata(key, expected) => json
            .get("metadata")
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
            .map(|v| v == expected)
            .unwrap_or(false),
        ScopeSpec::ByEpisode(ulid) => {
            json.get("id").and_then(|v| v.as_str()).map(|s| s == ulid.to_string()).unwrap_or(false)
        }
    }
}

/// W-1 fix: typed Hlc Ord compare on the `bt.valid.0` tuple element. The
/// previous draft used `format!("{hlc:?}")` lex compare which is incorrect
/// (lex compare on a struct's Debug repr ≠ field-wise Ord).
fn match_before(value: &[u8], hlc: Hlc) -> bool {
    let json: serde_json::Value = match serde_json::from_slice(value) {
        Ok(j) => j,
        Err(_) => return false,
    };
    json.get("bt")
        .and_then(|bt| bt.get("valid"))
        // BiTemporal::valid serializes as a 2-tuple `[Hlc, Option<Hlc>]`.
        // `.get(0)` indexes the first tuple element (valid_from).
        .and_then(|valid| valid.get(0))
        .and_then(|valid_from_json| {
            // W-1: typed deserialize + Ord compare. Hlc derives PartialOrd + Ord.
            serde_json::from_value::<Hlc>(valid_from_json.clone()).ok()
        })
        .map(|valid_from| valid_from < hlc)
        .unwrap_or(false)
}

/// Build the soft-delete WriteOp for one matched primitive.
///
/// - **B-4**: `clock.tick()` for the new HLC stamp; `Hlc::now()` does not
///   exist as a free function in this codebase.
/// - **B-5 (typed-first)**: mutate the in-memory BiTemporal via
///   `invalidate_sys(now)`, then JSON-patch the result into
///   `payload["bt"]["sys"][1]` so backends that derive persisted bt from the
///   payload bytes (Moon HSET layer, Postgres row storage) see the change.
///   Mirrors Plan 04-04 Task 4 `apply_supersede` (B-2 + B-2-RESIDUAL fix)
///   verbatim — `WriteOp::KvPut` has no separate `bt` field, so the mutation
///   MUST ride inside the value bytes.
fn build_soft_delete_op(m: &ForgetMatch, clock: &HlcClock) -> Result<WriteOp, LunarisError> {
    // B-4: clock.tick() — HlcClock method, not Hlc::now().
    let now = clock.tick();

    // B-5 typed-first: mutate the typed BiTemporal so any downstream code
    // using the returned struct sees the update. We then propagate the
    // mutation into the payload bytes below so Moon/Postgres pick it up.
    let mut bt = m.bt;
    bt.invalidate_sys(now);

    // B-5 JSON-patch: parse the existing payload, find the `bt` object's
    // `sys` tuple, and set the second element (sys_to) to Some(now). This is
    // the same pattern Plan 04-04 Task 4 uses for apply_supersede — the bt
    // field is derived from the payload bytes by Moon's HSET layer + the
    // Postgres row-storage layer, so a typed-only mutation is silently lost
    // unless we also patch the JSON.
    let mut json: serde_json::Value = serde_json::from_slice(&m.payload).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("forget payload parse: {e}")))
    })?;
    if let Some(bt_obj) = json.get_mut("bt")
        && let Some(sys_arr) = bt_obj.get_mut("sys")
        && let Some(arr) = sys_arr.as_array_mut()
        && arr.len() == 2
    {
        // Set the SECOND element (index 1 = sys_to) to the new
        // `now`. The first element (index 0 = sys_from) stays.
        arr[1] = serde_json::to_value(now).unwrap_or(serde_json::Value::Null);
    }
    let value = serde_json::to_vec(&json).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("forget payload serialize: {e}")))
    })?;
    Ok(WriteOp::KvPut { key: m.key.clone(), value })
}

/// All forget targets affect KV + vector + graph indices in v0. The
/// receipt carries this so callers can see the blast radius even on a
/// `dry_run`. Future targeted forms (e.g., "vector-only forget") would
/// return a narrower set.
fn classify_indices(target: &ForgetTarget) -> Vec<IndexKind> {
    match target {
        ForgetTarget::Id(_) | ForgetTarget::Scope(_) | ForgetTarget::Before(_) => {
            vec![IndexKind::Kv, IndexKind::Vector, IndexKind::Graph]
        }
    }
}

// ---------------------------------------------------------------------------
// Wave 1D — scope-aware forget pipeline (ADD task forget-scope-routing)
// ---------------------------------------------------------------------------

/// Scope-aware forget — the canonical pipeline behind
/// [`crate::handle::ScopedLunaris::forget`].
///
/// Same behaviour matrix as the deprecated dev-scope path (soft-default /
/// dry-run preview / hard-needs-token / one `atomic_write` per call / audit
/// on every call), with every storage operation routed through the CALLER's
/// scope: the Id fast path reads `keyspace::episode_key(scope, ulid)`, the
/// Scope/Before scan walks `keyspace::scope_prefix(scope) + "episode:"`, and
/// the write commits under `scope` — never `Scope::dev()`.
///
/// The 2026-07-14 live deep test proved the shim it replaces silently
/// returned `rows_written = 0` under every real scope (both prefix and
/// exact-ULID targets) because scan + write ran in the `_dev_` partition.
pub(crate) async fn forget_scoped(
    storage: &std::sync::Arc<dyn StoragePort>,
    clock: &HlcClock,
    scope: &lunaris_core::Scope,
    request: ForgetRequest,
) -> Result<ForgetReceipt, LunarisError> {
    let target_kind = match &request.target {
        ForgetTarget::Id(_) => "id",
        ForgetTarget::Scope(_) => "scope",
        ForgetTarget::Before(_) => "before",
    };
    let span = tracing::info_span!(
        "lunaris.forget",
        correlation_id = tracing::field::Empty,
        target = %target_kind,
        scope = %scope.as_str(),
        hard = %request.options.hard,
        dry_run = %request.options.dry_run,
    );
    async move {
        // B-3: hard delete must carry a confirmation token (D-21 rail).
        if request.options.hard && request.options.confirmation_token.is_none() {
            return Err(LunarisError::Validate(ValidateError::ConfirmationRequired(
                "hard-delete requires confirmation token from prior dry_run".into(),
            )));
        }

        let matches = scan_matches_scoped(storage.as_ref(), scope, &request.target, clock).await?;
        // W1.4: `matches` now holds chunk rows too. `matched` stays a count of
        // EPISODES so the preview keeps meaning what its docs say.
        let matched = episode_match_count(&matches, scope);

        if request.options.dry_run {
            let receipt = ForgetReceipt {
                target: request.target.clone(),
                indices_affected: classify_indices(&request.target),
                matched,
                rows_written: 0,
                rows_deleted: 0,
                audit_lsn: Lsn { wall_ms: 0, counter: 0 },
                preview: true,
            };
            let audit_offset = publish_audit_event(storage, AuditEvent::Forget((&receipt).into()))
                .await
                .unwrap_or(0);
            return Ok(ForgetReceipt {
                audit_lsn: Lsn { wall_ms: audit_offset, counter: 0 },
                ..receipt
            });
        }

        let ops: Vec<WriteOp> = if request.options.hard {
            matches.iter().map(|m| WriteOp::KvDelete { key: m.key.clone() }).collect()
        } else {
            matches.iter().map(|m| build_soft_delete_op(m, clock)).collect::<Result<Vec<_>, _>>()?
        };

        // D-19: at most ONE atomic_write per forget call — under the REAL scope.
        if !ops.is_empty() {
            let _lsn = storage.atomic_write(scope, &ops).await.map_err(LunarisError::Storage)?;
        }

        let receipt = ForgetReceipt {
            target: request.target.clone(),
            indices_affected: classify_indices(&request.target),
            matched,
            rows_written: if request.options.hard { 0 } else { matches.len() as u64 },
            rows_deleted: if request.options.hard { matches.len() as u64 } else { 0 },
            audit_lsn: Lsn { wall_ms: 0, counter: 0 },
            preview: false,
        };
        let audit_offset =
            publish_audit_event(storage, AuditEvent::Forget((&receipt).into())).await.unwrap_or(0);
        Ok(ForgetReceipt { audit_lsn: Lsn { wall_ms: audit_offset, counter: 0 }, ..receipt })
    }
    .instrument(span)
    .await
}

/// Scope-aware twin of [`scan_matches`]: keys are minted via
/// `lunaris_core::keyspace` (RC-1 — no local key formats) and every read
/// runs under the caller's scope.
async fn scan_matches_scoped(
    storage: &dyn StoragePort,
    scope: &lunaris_core::Scope,
    target: &ForgetTarget,
    clock: &HlcClock,
) -> Result<Vec<ForgetMatch>, LunarisError> {
    let mut out = scan_episode_matches_scoped(storage, scope, target, clock).await?;

    // W1.4: the episode row is not the content. Its chunks are, and until now
    // nothing in a forget ever touched them.
    //
    // A SOFT forget appeared to work because `hydrate`'s episode pass drops any
    // chunk whose parent episode is sys-closed. A HARD forget deletes the
    // episode row outright, so that lookup returns `None` instead of
    // `Some((_, true))` — and the gate stops firing. The chunk hydrates again,
    // with an empty `source`, after a confirmed irreversible delete that
    // reported `rows_deleted`.
    //
    // Reaching the chunks fixes both paths at once: soft stamps them, hard
    // deletes them, and neither depends on the episode row surviving.
    let episode_ids = matched_episode_ids(&out);
    if !episode_ids.is_empty() {
        out.extend(scan_chunk_matches_scoped(storage, scope, &episode_ids, clock).await?);
    }
    Ok(out)
}

/// The episode half — unchanged behaviour, lifted out so the chunk sweep below
/// runs for the `ForgetTarget::Id` fast path too.
async fn scan_episode_matches_scoped(
    storage: &dyn StoragePort,
    scope: &lunaris_core::Scope,
    target: &ForgetTarget,
    clock: &HlcClock,
) -> Result<Vec<ForgetMatch>, LunarisError> {
    use futures::stream::StreamExt;

    // OPS-01 fast path: single-target read_as_of on the canonical scoped key.
    if let ForgetTarget::Id(ulid) = target {
        let key = lunaris_core::keyspace::episode_key(scope, *ulid);
        let now = clock.tick();
        let row = storage.read_as_of(scope, &key, now).await.map_err(LunarisError::Storage)?;
        return Ok(match row {
            Some(r) => vec![ForgetMatch { key, payload: r.value.to_vec(), bt: r.bt }],
            None => Vec::new(),
        });
    }

    // OPS-02 / OPS-03: walk this scope's episode partition.
    let prefix = format!("{}episode:", lunaris_core::keyspace::scope_prefix(scope)).into_bytes();

    let mut stream =
        storage.scan_range(scope, &prefix, None).await.map_err(LunarisError::Storage)?;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let (k, v) = item.map_err(LunarisError::Storage)?;
        if matches_target(&k, &v, target) {
            let now = clock.tick();
            let row_opt =
                storage.read_as_of(scope, &k, now).await.map_err(LunarisError::Storage)?;
            let bt = row_opt
                .as_ref()
                .map(|r| r.bt)
                .unwrap_or_else(|| BiTemporal { valid: (Hlc::ZERO, None), sys: (Hlc::ZERO, None) });
            out.push(ForgetMatch { key: k.to_vec(), payload: v.to_vec(), bt });
        }
    }
    Ok(out)
}

/// How many EPISODES a scan matched, as opposed to how many rows it will touch.
///
/// Counted by key prefix rather than by payload shape: a `Chunk` also carries
/// an `id` field, so the payload-parsing sibling below would happily count one.
/// The key is the only thing that says which kind a row is.
fn episode_match_count(matches: &[ForgetMatch], scope: &lunaris_core::Scope) -> u64 {
    let prefix = lunaris_core::keyspace::episode_prefix(scope);
    matches.iter().filter(|m| m.key.starts_with(&prefix)).count() as u64
}

/// The ulids of the episodes a scan matched, read from the payloads.
///
/// Parsed from the value rather than the key: the key format belongs to
/// `lunaris_core::keyspace` and this module must not re-derive it (RC-1).
fn matched_episode_ids(matches: &[ForgetMatch]) -> std::collections::HashSet<Ulid> {
    matches
        .iter()
        .filter_map(|m| serde_json::from_slice::<serde_json::Value>(&m.payload).ok())
        .filter_map(|v| v.get("id").and_then(|id| id.as_str()).and_then(|s| s.parse::<Ulid>().ok()))
        .collect()
}

/// Every chunk row belonging to one of `episode_ids`.
///
/// This is a prefix scan of the scope's chunk partition, and it runs even on
/// the `ForgetTarget::Id` fast path, which is a real cost: ingest writes
/// doctree / episode / chunk / vector ops and **no** reverse episode->chunk
/// index, so there is nothing cheaper to consult. Forget is not a hot path and
/// correctness wins here; a back-link written at ingest would remove the scan
/// and is the obvious follow-up if a large scope ever makes this hurt.
///
/// Deliberately NOT extended to facts, entities, relations or communities.
/// Those are keyed by a content hash — `FactId = blake3(subject || predicate
/// || object)`, `EntityId = blake3(name || type)` — so two episodes asserting
/// the same thing write the SAME row, and no provenance links a row back to
/// the episodes that contributed it (`RawExtraction::source_chunk_id` is
/// dropped when the validated `Fact` is persisted). Deleting one of those on a
/// single episode's forget would erase another episode's assertion. Doing it
/// properly needs contributing-episode provenance plus reference-counted
/// deletion — a schema change, tracked separately.
async fn scan_chunk_matches_scoped(
    storage: &dyn StoragePort,
    scope: &lunaris_core::Scope,
    episode_ids: &std::collections::HashSet<Ulid>,
    clock: &HlcClock,
) -> Result<Vec<ForgetMatch>, LunarisError> {
    use futures::stream::StreamExt;

    let prefix = lunaris_core::keyspace::chunk_prefix(scope);
    let mut stream =
        storage.scan_range(scope, &prefix, None).await.map_err(LunarisError::Storage)?;
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        let (k, v) = item.map_err(LunarisError::Storage)?;
        let Ok(chunk) = serde_json::from_slice::<lunaris_core::Chunk>(&v) else {
            continue; // not a chunk row, or a shape this version cannot read
        };
        if !episode_ids.contains(&chunk.episode_id) {
            continue;
        }
        // Same typed-bt load the episode loop does: `build_soft_delete_op`
        // mutates the typed BiTemporal before patching it into the payload.
        let now = clock.tick();
        let row_opt = storage.read_as_of(scope, &k, now).await.map_err(LunarisError::Storage)?;
        let bt = row_opt
            .as_ref()
            .map(|r| r.bt)
            .unwrap_or_else(|| BiTemporal { valid: (Hlc::ZERO, None), sys: (Hlc::ZERO, None) });
        out.push(ForgetMatch { key: k.to_vec(), payload: v.to_vec(), bt });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_builder_sets_options() {
        let req = ForgetTarget::Id(Ulid::new()).dry_run();
        assert!(req.options.dry_run);
        assert!(!req.options.hard);
        assert!(req.options.confirmation_token.is_none());
    }

    #[test]
    fn hard_builder_sets_options() {
        let req = ForgetTarget::Id(Ulid::new()).hard();
        assert!(req.options.hard);
        assert!(!req.options.dry_run);
        assert!(req.options.confirmation_token.is_none());
    }

    #[test]
    fn with_token_attaches_confirmation() {
        let token = ForgetConfirmation { for_audit_lsn: Lsn { wall_ms: 42, counter: 0 } };
        let req = ForgetTarget::Id(Ulid::new()).hard().with_token(token.clone());
        assert_eq!(req.options.confirmation_token, Some(token));
    }

    #[test]
    fn classify_indices_returns_three_indices() {
        let v = classify_indices(&ForgetTarget::Id(Ulid::new()));
        assert!(v.contains(&IndexKind::Kv));
        assert!(v.contains(&IndexKind::Vector));
        assert!(v.contains(&IndexKind::Graph));
    }

    #[test]
    fn match_scope_by_source_prefix() {
        let payload = serde_json::to_vec(&serde_json::json!({
            "source": "helios:fs/session-42/foo.md",
        }))
        .unwrap();
        assert!(match_scope(&payload, &ScopeSpec::BySource("helios:fs/session-42/".into())));
        assert!(!match_scope(&payload, &ScopeSpec::BySource("helios:fs/session-99/".into())));
    }

    #[test]
    fn match_scope_by_metadata_exact() {
        let payload = serde_json::to_vec(&serde_json::json!({
            "metadata": { "tenant": "acme" },
        }))
        .unwrap();
        assert!(match_scope(&payload, &ScopeSpec::ByMetadata("tenant".into(), "acme".into())));
        assert!(!match_scope(&payload, &ScopeSpec::ByMetadata("tenant".into(), "other".into())));
    }

    /// W-1: typed Hlc Ord compare on the valid-from tuple element.
    #[test]
    fn match_before_uses_typed_hlc_compare() {
        let earlier = Hlc { wall_ms: 100, counter: 0, node_id: 0 };
        let later = Hlc { wall_ms: 200, counter: 0, node_id: 0 };
        let cutoff = Hlc { wall_ms: 150, counter: 0, node_id: 0 };

        let payload_earlier = serde_json::to_vec(&serde_json::json!({
            "bt": { "valid": [earlier, null], "sys": [earlier, null] }
        }))
        .unwrap();
        let payload_later = serde_json::to_vec(&serde_json::json!({
            "bt": { "valid": [later, null], "sys": [later, null] }
        }))
        .unwrap();

        assert!(match_before(&payload_earlier, cutoff), "earlier < cutoff");
        assert!(!match_before(&payload_later, cutoff), "later >= cutoff");
    }

    #[test]
    fn forget_target_into_request_defaults_to_soft() {
        let req: ForgetRequest = ForgetTarget::Id(Ulid::new()).into_request();
        assert!(!req.options.hard);
        assert!(!req.options.dry_run);
    }

    #[test]
    fn from_impl_allows_bare_target_in_forget_arg() {
        let req: ForgetRequest = ForgetTarget::Id(Ulid::new()).into();
        assert!(!req.options.hard);
    }

    /// B-5: build_soft_delete_op JSON-patches `payload["bt"]["sys"][1]`. Set
    /// up a payload with a tuple-shaped bt object and verify the patched
    /// payload has sys[1] non-null after a build_soft_delete_op pass.
    #[test]
    fn build_soft_delete_op_patches_bt_sys_to() {
        let t0 = Hlc { wall_ms: 1, counter: 0, node_id: 0 };
        let payload = serde_json::to_vec(&serde_json::json!({
            "id": Ulid::new().to_string(),
            "source": "test:src",
            "bt": { "valid": [t0, null], "sys": [t0, null] },
        }))
        .unwrap();
        let bt = BiTemporal { valid: (t0, None), sys: (t0, None) };
        let m = ForgetMatch { key: b"episode:test".to_vec(), payload, bt };
        let clock = HlcClock::new(0);
        let op = build_soft_delete_op(&m, clock.as_ref()).expect("build_soft_delete_op");
        match op {
            WriteOp::KvPut { key, value } => {
                assert_eq!(key, b"episode:test");
                let json: serde_json::Value = serde_json::from_slice(&value).unwrap();
                let sys_to = json
                    .get("bt")
                    .and_then(|bt| bt.get("sys"))
                    .and_then(|sys| sys.get(1))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                assert!(!sys_to.is_null(), "sys[1] (sys_to) MUST be patched non-null");
            }
            other => panic!("expected KvPut, got {other:?}"),
        }
    }
}

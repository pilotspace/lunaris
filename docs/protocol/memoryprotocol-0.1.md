# MemoryProtocol 0.1

**Status:** alpha
**Source of truth:** this document
**Conformance harness:** [`lunaris-conformance::protocol`](../../crates/lunaris-conformance/src/protocol/)
**Reference implementation:** [`lunaris-server`](../../crates/lunaris-server/) (axum 0.8 binary)
**Spec authors:** Lunaris team

MemoryProtocol is the HTTP+SSE wire protocol an agent harness uses to talk to a Lunaris memory engine. It exposes the blueprint §5.4 verbs (`POST /v1/ingest`, `POST /v1/recall`, `POST /v1/forget`, `GET /v1/snapshot/:lsn`, `GET /v1/episode/:id`) plus a Prometheus `/metrics` endpoint (Plan 05-05) and a no-auth `/healthz` probe. v0 is JSON-only over HTTP/1.1 + HTTP/2; bincode/CBOR/MessagePack wire formats are deferred to v1 (`PROTO-V1-02`-equivalent).

The implementation under test for the conformance harness is `lunaris-server` from this workspace. Third-party servers (Go, Python, etc.) are conformant if and only if they satisfy [§Conformance](#conformance).

## Versioning

- The `/v1/` path prefix is stable. v0 is the alpha; v1 is the GA. Future incompatible changes get `/v2/`.
- Backwards-compatible additions land under `/v1/` (new optional fields, new endpoints).
- The conformance gate certifies any implementation against this document.

## Authentication

Bearer token in the `Authorization` header on every `/v1/*` request:

```
Authorization: Bearer <token>
```

Tokens are mapped to a tenant id + scope set via the server's `--tokens-file` flag. The map shape (CONTEXT.md D-07 verbatim):

```json
{
  "<token>": { "tenant": "<id>", "scopes": ["ingest", "recall", "forget"] }
}
```

- Missing or malformed `Authorization` header → `401 Unauthorized`.
- Token not in map → `401 Unauthorized`.
- Token present but lacks the required scope for the route → `403 Forbidden`.

Per-route scope requirements:

| Route                       | Required scope |
|-----------------------------|----------------|
| `POST /v1/ingest`           | `ingest`       |
| `POST /v1/recall`           | `recall`       |
| `POST /v1/forget`           | `forget`       |
| `GET  /v1/snapshot/{lsn}`   | `recall`       |
| `GET  /v1/episode/{id}`     | `recall`       |
| `GET  /healthz`             | (none)         |
| `GET  /metrics`             | (none — Plan 05-05) |

OAuth2 / JWT / OIDC issuance is a v1 gate (managed cloud `DEPLOY-V1-01`).

> **"JWT" below is historical wording.** v0 ships **opaque bearer tokens** —
> the claims (`tenant` = partition scope, `scopes` = verb permissions) live in
> the server-side tokens file, never in the token. Read every "JWT `tenant`
> claim" in this document as "the `tenant` claim the server resolved for this
> token". Managed JWT/OIDC issuance is the v1 gate `DEPLOY-V1-01`.

## Rate limiting

Per-tenant rate limit applied to every `/v1/*` request. Defaults: `60 rps`, `120 burst` (configurable via `--rate-per-second` / `--rate-burst`). The conformance harness configures `5 rps / 10 burst` so the burst test fires within hundreds of milliseconds.

- Exceeded → `429 Too Many Requests` with a `Retry-After: <seconds>` header.

The key extractor reads the `tenant` field from the validated bearer token's `AuthClaims`; un-authenticated routes are not rate-limited (in v0 there are no un-authenticated `/v1/*` routes).

## Verbs

### POST /v1/ingest

Ingest one Episode. The server fans out chunking + embedding + atomic write internally. Single atomic-write invariant preserved (`INGEST-04`); the HTTP layer adds NO new atomic boundaries.

**Required scope:** `ingest`

**Request body** (`application/json`):

```json
{
  "id":       "01JBA...",                   // ULID, optional (server generates if absent)
  "source":   "helios:fs/notes.md",         // string, required
  "content":  "Markdown body up to ~12 KB", // string, required
  "t_ref":    "2026-04-21T10:30:00Z",       // RFC-3339, optional (defaults to wall clock)
  "metadata": { "any": "json" }             // object, optional
}
```

**Response** (`200 OK`, `application/json`):

```json
{
  "lsn":            { "wall_ms": 1745251800123, "counter": 0 },
  "queue_lag_warn": false
}
```

`queue_lag_warn` is `true` when the verifier-queue depth (`StoragePort::queue_depth("__lunaris_verify__", 0)`) exceeds 1000 (the `DEFAULT_VERIFY_WARN_THRESHOLD` from `crates/lunaris/src/recall.rs`). Best-effort: backends without `queue_depth` report `false`.

**Errors:** `400` (invalid Episode JSON), `401` / `403` (auth), `429` (rate), `500` (storage).

### POST /v1/recall

Run a hybrid retrieval (Vector + Keyword(BM25) + RRF + bge-rerank). Two retrieval modes per blueprint §5.4 (CONTEXT.md D-05): `semantic` (default) and `graph` (anchored Cypher BFS, requires `capabilities().graph_native` OR runtime `GraphPipeline::enable()`).

**Required scope:** `recall`

**Request body** (`application/json`):

```json
{
  "query":  "When did Alice join Acme?", // string, required
  "k":      10,                          // usize, default 10
  "as_of":  "2025-06-01T00:00:00Z",      // RFC-3339, optional (server parses to Hlc)
  "filter": "source LIKE 'helios:fs/%'", // v0 filter DSL, optional
  "mode":   "semantic"                   // "semantic" | "graph", default "semantic"
}
```

**Response (default — `Accept: application/json`)** (`200 OK`):

```json
[
  {
    "id":             [/* bytes */],
    "score":          0.93,
    "text":           "Alice joined Acme on 2024-08-12.",
    "source":         "helios:fs/notes.md",
    "heading_path":   ["onboarding"],
    "valid_from":     { "wall_ms": ..., "counter": 0, "node_id": 0 },
    "valid_to":       null,
    "degraded":       false,
    "rerank_applied": true,
    "source_op":      "Reranked"
  }
]
```

**Response (SSE — `Accept: text/event-stream`)** (`200 OK`, `Content-Type: text/event-stream`):

```
event: hit
data: { "id": [...], "score": 0.93, "degraded": false, ... }

event: hit
data: { ... }

event: done
data: {}

```

Each event carries `event:` + `data:` lines per W3C SSE. The stream terminates with `event: done`. The `degraded` flag is populated from the Phase 4 verifier-queue depth check (`recall_with_degraded_check` in `crates/lunaris/src/recall.rs`); the SSE stream surfaces it per-Hit.

A keep-alive comment is emitted every 15 seconds while the stream is idle so reverse proxies don't time out.

**Errors:** `400` (invalid request body or filter DSL parse error), `401`, `403`, `429`, `500`, `501` (graph mode requested but `!capabilities().graph_native && !graph_pipeline().is_enabled()`).

### POST /v1/forget

Single-target / scope / temporal-bound purge. Two-step hard-delete safety rail per Plan 04-05 D-21.

**Required scope:** `forget`

**Request body** (`application/json`):

```json
{
  "target": { "Id": "01JBA..." },
  // OR { "Scope": { "BySource": "helios:fs/session-42/" } }
  // OR { "Before": { "wall_ms": ..., "counter": ... } }
  "hard":               false,  // bool, default false (soft-delete via MVCC)
  "dry_run":            false,  // bool, default false (returns preview-only ForgetReceipt)
  "confirmation_token": null    // string, REQUIRED when hard=true
}
```

**Response** (`200 OK`):

```json
{
  "target":           { "Id": "..." },
  "indices_affected": ["Kv", "Vector", "Graph"],
  "rows_written":     1,
  "rows_deleted":     0,
  "audit_lsn":        { "wall_ms": ..., "counter": ... },
  "preview":          false
}
```

**D-21 two-step hard-delete contract:**

1. POST `/v1/forget` with `dry_run: true` + the target → `200 OK` + `ForgetReceipt { preview: true, ... }`.
2. POST `/v1/forget` with `hard: true` + the target + `confirmation_token: <stringified-receipt-from-step-1>` → `200 OK` + `ForgetReceipt { preview: false, rows_deleted: N }`.

The wire shape for `confirmation_token` is the SERIALIZED prior `ForgetReceipt` JSON. The Rust API's `ForgetConfirmation` has a `pub(crate)` inner field, so external HTTP callers cannot mint the typed token directly; the server deserializes the prior receipt + calls `Lunaris::confirm_hard_forget` to mint the typed token before re-issuing the hard delete (Plan 05-01 routes/forget.rs).

Without step 1, step 2 returns `428 Precondition Required`:

```json
{ "error": "confirmation_required",
  "message": "hard-delete requires confirmation_token (serialized prior ForgetReceipt JSON)" }
```

A malformed `confirmation_token` body field returns `400 invalid_confirmation_token`:

```json
{ "error": "invalid_confirmation_token",
  "message": "confirmation_token must be a serialized ForgetReceipt: <parser error>" }
```

Every successful `forget` call publishes one `AuditEvent::Forget` to `__lunaris_audit__` (best-effort, fire-and-forget per Plan 04-05 OPS-04).

### GET /v1/snapshot/{lsn}

Stream every primitive at the given Lsn as newline-delimited JSON (`application/x-ndjson`).

**Required scope:** `recall`

**Path param:** `{lsn}` is the Hlc encoded as `<wall_ms>.<counter>` (decimal pair), or `<wall_ms>.<counter>.<node_id>` (decimal triple). Examples: `/v1/snapshot/1745251800123.0`, `/v1/snapshot/1745251800123.5.0`.

**Response** (`200 OK`, `Content-Type: application/x-ndjson`):

```
{"key":"chunk:01JBA...","value":{"id":[...],"text":"...","bt":{...}}}
{"key":"entity:01JBA...","value":{"id":[...],"label":"...","bt":{...}}}
...
```

One JSON object per line. Stream may be empty on a fresh backend.

**Errors:**
- `400 invalid_lsn` — path param is not in `wall_ms.counter[.node_id]` form.
- `404 snapshot_out_of_range` — `{lsn}` wall_ms is **strictly greater** than the engine's current wall clock. A past LSN with zero visible rows returns `200` + empty NDJSON (valid empty snapshot, not "not found").
- `401`, `403`, `429`, `500`.

### GET /v1/episode/{id}

Fetch a single episode by ULID from the caller's JWT-bound scope.

**Required scope:** `recall`

**Path param:** `{id}` is a 26-character Crockford base-32 ULID string (e.g. `01HZZZZZZZZZZZZZZZZZZZZZZZ`).

**Response** (`200 OK`, `application/json`):

The stored episode value as a JSON object (the same bytes written by `POST /v1/ingest`).

```json
{
  "id":       "01HZZZZZZZZZZZZZZZZZZZZZZZ",
  "source":   "helios:fs/notes.md",
  "content":  "...",
  "metadata": { "any": "json" }
}
```

**Errors:**
- `400 invalid_episode_id` — `{id}` is not a valid 26-character Crockford base-32 ULID.
- `404 episode_not_found` — no episode with that ULID exists in the caller's scope.
- `401`, `403`, `429`, `500`.

The JWT `tenant` claim is the exclusive scope partition key; no wire-side `scope` field is accepted. The KV key is constructed as `lunaris:{scope}:episode:{ulid}` (canonical format per `lunaris_core::keyspace::episode_key`).

### GET /healthz

No auth, no rate-limit. Probe surface for load balancers + the conformance subprocess runner.

**Response** (`200 OK`):

```json
{ "ok": true, "version": "0.1.0-alpha.1" }
```

### GET /metrics

Prometheus text-format exposition. **No auth required** — Prometheus scrapers reach this without a Bearer token. Operators MUST front this endpoint with network-level ACL or reverse-proxy auth in production (T-05-05-05; standard Prometheus convention).

Returns `404 Not Found` when `lunaris-server --metrics-disabled` is set at startup (operator opt-out for embedded deployments that scrape via a sidecar).

**Metrics catalogue** (Plan 05-05 OPS-06; CONTEXT.md D-25 verbatim):

| Name                                | Type      | Labels                              | Notes                                                                          |
|-------------------------------------|-----------|-------------------------------------|--------------------------------------------------------------------------------|
| `lunaris_ingest_total`              | counter   | `tenant`, `status`                  | One increment per `POST /v1/ingest`; `status` ∈ {`ok`, `error`}.               |
| `lunaris_ingest_duration_seconds`   | histogram | `tenant`                            | Wall-clock from request entry to response sent (includes business logic).      |
| `lunaris_recall_total`              | counter   | `tenant`, `mode`, `status`          | `mode` ∈ {`semantic`, `graph`}; `status` ∈ {`ok`, `error`}.                    |
| `lunaris_recall_duration_seconds`   | histogram | `tenant`, `mode`                    | Wall-clock; same shape as ingest_duration.                                     |
| `lunaris_forget_total`              | counter   | `tenant`, `target_kind`, `hard`     | `target_kind` ∈ {`id`, `scope`, `before`}; `hard` ∈ {`true`, `false`}.         |
| `lunaris_verify_queue_depth`        | gauge     | `topic`                             | Polled every 10 s from `StoragePort::queue_depth("__lunaris_verify__", 0)`.    |
| `lunaris_consolidator_queue_depth`  | gauge     | `topic`                             | Polled every 10 s from `StoragePort::queue_depth("__lunaris_consolidate__", 0)`. |
| `lunaris_error_total`               | counter   | `kind`                              | `LunarisError` variant tag; cardinality cap ≤ 10. Incremented inside `map_error`. |
| `lunaris_eval_score`                | gauge     | `harness`                           | Populated by `lunaris-evals` (Plan 05-06); harness ∈ {`longmemeval`, `locomo`, `er-f1`, …}. |

**Cardinality bounds** (T-05-05-02 mitigation):
- `tenant` set membership = `--tokens-file` JSON map size (operator-controlled).
- All other labels are bounded by the constants above; the total time series count grows linearly with tenant count, NOT with traffic volume.

**Content-Type:** `text/plain; version=0.0.4; charset=utf-8` (the `prometheus::TextEncoder::format_type()` value). Body parses via any standard Prometheus scraper or `prometheus-client` library.

## Error taxonomy

| HTTP status | Server cause                          | `LunarisError` variant                      | JSON body shape                                                |
|-------------|---------------------------------------|---------------------------------------------|----------------------------------------------------------------|
| 400         | Bad request body / filter DSL         | `LunarisError::Validate(_)`                 | `{ "error": "validate", "message": "..." }`                    |
| 400         | Bad confirmation_token JSON           | n/a (handler-local validation)              | `{ "error": "invalid_confirmation_token", "message": "..." }`  |
| 400         | Bad RFC-3339 `as_of`                  | n/a (handler-local validation)              | `{ "error": "invalid_request", "message": "..." }`             |
| 400         | Bad NDJSON snapshot Lsn               | n/a (handler-local validation)              | `{ "error": "invalid_lsn", "message": "..." }`                 |
| 400         | Bad episode ULID path param           | n/a (handler-local validation)              | `{ "error": "invalid_episode_id", "message": "..." }`          |
| 401         | Missing / invalid bearer              | n/a (auth middleware)                       | `{ "error": "unauthorized", "message": "..." }`                |
| 403         | Token lacks required scope            | n/a (auth middleware)                       | `{ "error": "forbidden", "message": "..." }`                   |
| 404         | Snapshot LSN wall_ms strictly future  | n/a (handler-local validation)              | `{ "error": "snapshot_out_of_range", "message": "..." }`       |
| 404         | Episode not found in caller's scope   | n/a (handler-local: `read_as_of` → `None`) | `{ "error": "episode_not_found", "message": "..." }`           |
| 428         | Hard-delete without confirmation      | `LunarisError::Validate(ValidateError::ConfirmationRequired(_))` | `{ "error": "confirmation_required", "message": "..." }` |
| 429         | Rate limit exceeded                   | n/a (tower-governor middleware)             | (empty body; `Retry-After` header)                             |
| 500         | Storage / extract / consolidate error | `LunarisError::Storage(_)` / `::Extract(_)` / `::Consolidate(_)` | `{ "error": "storage" \| "extract" \| "consolidate", "message": "..." }` |
| 501         | Capability missing (graph mode)       | n/a (handler-local capability check)        | `{ "error": "graph_mode_unavailable", "message": "..." }`      |

The `LunarisError` enum is defined in `crates/lunaris-core/src/error.rs`; the HTTP mapping lives in `crates/lunaris-server/src/middleware/error.rs::map_error`.

## Conformance

An implementation is conformant if and only if:

```bash
MOON_URL=moon://localhost:6380 \
  cargo test -p lunaris-conformance \
    --test run_protocol_lunaris_server -- --nocapture
```

returns exit code `0`. The harness exercises every contract in [§Verbs](#verbs) + [§Authentication](#authentication) + [§Rate limiting](#rate-limiting) + [§Error taxonomy](#error-taxonomy).

See [`conformance.md`](./conformance.md) for the full how-to (storage suite, protocol suite, AS_OF behaviour, third-party certification).

The `memoryprotocol.dev` site standup with publicly-hosted certifications is a v1 deliverable (`PROTO-V1-01`).

## Glossary

- **Lsn** — `{ wall_ms: u64, counter: u32 }`. Returned by `atomic_write`.
- **Hlc** — Hybrid Logical Clock; `{ wall_ms: u64, counter: u32, node_id: u16 }`. Used in `as_of`, `bt`.
- **`bt`** — bi-temporal stamp `{ valid: (Hlc, Option<Hlc>), sys: (Hlc, Option<Hlc>) }`.
- **`StorageCapabilities`** — backend feature report; gates capability-conditional behavior (graph mode, queue mode, native RRF).
- **Episode** — input observation; chunked + embedded server-side into one or more primitives.
- **Hit** — output of `recall`; carries chunk text + `degraded` flag + `valid_from` / `valid_to`.
- **`ForgetReceipt`** — output of `forget`; carries `indices_affected`, `rows_written`, `rows_deleted`, `audit_lsn`, `preview`.

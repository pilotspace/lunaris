# Running the HTTP Server

**Reach for `lunaris-server` when an agent harness — or a non-Rust service —
needs to talk to a shared Lunaris memory engine over HTTP/SSE instead of
linking the crate.** It is the reference implementation of
[MemoryProtocol 0.1](../protocol/memoryprotocol-0.1.md): an axum 0.8 binary
exposing the blueprint verbs plus a Prometheus `/metrics` endpoint.

The crate is `lunaris-server`; config lives in
`crates/lunaris-server/src/config.rs`. The full env-var / feature-flag matrix
is in the [Configuration Reference](../reference/configuration.md) — this page
covers operating the binary.

## Quick start

```bash
cargo build -p lunaris-server          # → target/debug/lunaris-server

LUNARIS_STORAGE=moon://localhost:6380 \
LUNARIS_TOKENS_FILE=/etc/lunaris/tokens.json \
  target/debug/lunaris-server --bind 127.0.0.1:8080
```

On startup the binary prints its bound address to stderr as
`LISTENING_ON <addr>` (e.g. `LISTENING_ON 127.0.0.1:8080`) — the conformance
subprocess runner parses this; see `crates/lunaris-server/src/main.rs`. Then
probe it:

```bash
curl -s localhost:8080/healthz                                  # {"ok":true,"version":"0.2.1"}  (version = CARGO_PKG_VERSION)
curl -s -H 'Authorization: Bearer <token>' \
     -H 'Content-Type: application/json' \
     -d '{"source":"demo","content":"Alice loves chocolate."}' \
     localhost:8080/v1/ingest
```

## Configuration

Every flag has a matching `LUNARIS_*` env var (12-factor; CLI flag wins over
env). Source: `crates/lunaris-server/src/config.rs`.

| Flag / env var | Default | Meaning |
|---|---|---|
| `--bind` / `LUNARIS_BIND` | `0.0.0.0:8080` | Listen address |
| `--storage` / `LUNARIS_STORAGE` | *(required)* | Storage URL — `moon://host:port` or `postgres://user:pass@host/db`; the scheme picks the backend (see [Choosing a Backend](./backends.md)) |
| `--tokens-file` / `LUNARIS_TOKENS_FILE` | *(required)* | Path to the bearer-token map JSON (below) |
| `--rate-per-second` / `LUNARIS_RATE_PER_SECOND` | `60` | Per-tenant sustained request rate |
| `--rate-burst` / `LUNARIS_RATE_BURST` | `120` | Per-tenant burst budget |
| `--cors-origins` / `LUNARIS_CORS_ORIGINS` | `*` | CORS allow-list — `*` or a comma-separated origin list |
| `--shutdown-grace-secs` / `LUNARIS_SHUTDOWN_GRACE_SECS` | `30` | Graceful-shutdown drain window |
| `--metrics-disabled` | *(off)* | Remove the `/metrics` endpoint (no env var) |

The same `LUNARIS_EMBEDDER_BACKEND` / `LUNARIS_GRAPH_ENABLED` / verifier /
consolidator env vars that `Lunaris::open` reads also apply here — see
[Configuration Reference §2](../reference/configuration.md#2-environment-variables).

### The bearer-token map (`LUNARIS_TOKENS_FILE`)

A JSON object mapping opaque bearer tokens to a tenant id + scope set
(CONTEXT.md D-07):

```json
{
  "<opaque-bearer-token>": { "tenant": "acme",   "scopes": ["ingest", "recall", "forget"] },
  "<another-token>":       { "tenant": "globex", "scopes": ["recall"] }
}
```

- `tenant` is the **partition scope** for the token (typed and validated as a
  `Scope`) — the **only** source of truth for it. Route handlers consume the
  token-bound scope and **ignore** any `scope` / `tenant` field on the request
  body; every public DTO carries `#[serde(deny_unknown_fields)]`, so a request
  body that *contains* such a field is rejected outright (HTTP 422).
- `scopes` is the **verb-permission set** for the token — which of `ingest` /
  `recall` / `forget` it may call. A request whose route requires a verb the
  token doesn't carry → `403 Forbidden`.
- A missing or invalid token → `401 Unauthorized`.

> **Heads-up on `forget` under real scopes.** In v0.2.x, `Lunaris::forget`
> still routes through `Scope::dev()` internally for its `atomic_write` /
> `read_as_of` / `scan_range` calls — a `forget` issued under any non-`_dev_`
> scope silently returns `rows_written = 0`, `rows_deleted = 0` (Postgres RLS /
> the Moon SCAN prefix filter everything out). It emits a `tracing::warn!` on
> every call. The real per-scope routing — `ScopedLunaris::forget(target)` with
> a `403`/`404` cross-scope contract — is a **v0.3 deliverable** (RFC 0001
> §11.6, `CHANGELOG.md` "Known issues"). See [Forgetting](../guides/forget.md)
> and `docs/migration/0.1-to-0.2.md` §10.2.

## Endpoints (operational view)

The wire spec is [MemoryProtocol 0.1](../protocol/memoryprotocol-0.1.md); the
operational summary:

| Route | Required scope | What it does |
|---|---|---|
| `POST /v1/ingest` | `ingest` | Ingest one Episode; server chunks + embeds + does **one** `atomic_write`. Returns `{lsn, queue_lag_warn}`. |
| `POST /v1/recall` | `recall` | Hybrid retrieval (Vector + BM25 + RRF + optional rerank). `Accept: application/json` → array of hits; `Accept: text/event-stream` → SSE stream (`event: hit` … `event: done`, 15 s keep-alive). `mode: "graph"` needs a graph-capable backend or `GraphPipeline::enable()` (else `501`). |
| `POST /v1/forget` | `forget` | Single-target / by-source / temporal-bound purge. Two-step hard-delete rail: `dry_run:true` → preview receipt; then `hard:true` + `confirmation_token: <serialized prior receipt>` → real delete. `hard:true` without the token → `428 Precondition Required`. |
| `GET /v1/snapshot/{lsn}` | `recall` | Streams every primitive at the given Hlc (`<wall_ms>.<counter>[.<node_id>]`) as `application/x-ndjson`. Returns `404 snapshot_out_of_range` if the wall_ms is strictly in the future; an empty past snapshot is `200` + empty body. |
| `GET /v1/episode/{id}` | `recall` | Fetch a single episode by ULID from the caller's scope. `200` + JSON on hit; `400 invalid_episode_id` on malformed ULID; `404 episode_not_found` when absent. |
| `GET /healthz` | *(none)* | LB probe — `{"ok":true,"version":...}`. No auth, not rate-limited. |
| `GET /metrics` | *(none)* | Prometheus text exposition. **No auth** — front it with a network ACL or reverse-proxy auth. `404` when `--metrics-disabled`. |

### Rate limiting

Per-tenant, applied to every `/v1/*` request (key = the `tenant` claim).
Exceeded → `429 Too Many Requests` with a `Retry-After: <seconds>` header.
Un-authenticated routes (`/healthz`, `/metrics`) are not rate-limited.

### Metrics

`/metrics` exposes (Plan 05-05; CONTEXT.md D-25):

| Metric | Type | Labels |
|---|---|---|
| `lunaris_ingest_total` | counter | `tenant`, `status` |
| `lunaris_ingest_duration_seconds` | histogram | `tenant` |
| `lunaris_recall_total` | counter | `tenant`, `mode`, `status` |
| `lunaris_recall_duration_seconds` | histogram | `tenant`, `mode` |
| `lunaris_forget_total` | counter | `tenant`, `target_kind`, `hard` |
| `lunaris_verify_queue_depth` | gauge | `topic` |
| `lunaris_consolidator_queue_depth` | gauge | `topic` |
| `lunaris_error_total` | counter | `kind` (cardinality cap ≤ 10) |
| `lunaris_eval_score` | gauge | `harness` |

Time-series count grows linearly with **tenant count** (the tokens-file map
size), not with traffic. `Content-Type` is the standard
`text/plain; version=0.0.4; charset=utf-8`.

## Deployment notes

- **Stateless process.** Every byte of durable state lives in the backend;
  the server holds nothing on disk. Scale horizontally; restarts are free.
  See [Durability & Recovery](./durability.md).
- **Graceful shutdown.** On `SIGTERM`/`SIGINT` the server stops accepting new
  connections and drains in-flight requests for `--shutdown-grace-secs`
  (default 30 s) before exiting. Set your orchestrator's termination grace
  period at least that high.
- **HTTP-only image.** A `cargo build --no-default-features -p lunaris`
  build links neither the ONNX nor the candle stack — useful when the server
  only talks to a remote embedder (Ollama / cloud-API) and you want a small
  image. Pick the embedder via `LUNARIS_EMBEDDER_BACKEND`.
- **TLS / OAuth.** v0 is plain HTTP with opaque bearer tokens; terminate TLS
  and do OAuth2/OIDC issuance at a reverse proxy. Managed-cloud JWT issuance
  is a v1 gate (`DEPLOY-V1-01`).
- **Errors.** The HTTP status ↔ `LunarisError` mapping lives in
  `crates/lunaris-server/src/middleware/error.rs::map_error`; the full table
  is in [the protocol spec](../protocol/memoryprotocol-0.1.md#error-taxonomy)
  and [Error Taxonomy](../reference/errors.md).

## See also

- [MemoryProtocol 0.1](../protocol/memoryprotocol-0.1.md) — the wire spec
- [Conformance](../protocol/conformance.md) — certifying a server
- [Choosing a Backend](./backends.md) — Moon vs Postgres
- [Configuration Reference](../reference/configuration.md) — every flag/var

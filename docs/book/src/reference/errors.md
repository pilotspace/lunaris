# Error Taxonomy

**Every public `lunaris` API returns `Result<_, LunarisError>`.** `LunarisError`
is one umbrella enum with a sub-enum per subsystem; the HTTP server maps each
variant onto a status code (see [Protocol → MemoryProtocol 0.1](../protocol/memoryprotocol-0.1.md)).

Source: `crates/lunaris-core/src/error.rs`.

## `LunarisError` (umbrella)

`#[non_exhaustive]` — always include a wildcard arm when matching; new
subsystems can be added in a patch release.

| Variant | Wraps | Meaning |
|---|---|---|
| `Storage(StorageError)` | backend / scheme / (de)serialization / IO faults | the KV-vector-graph-queue substrate failed or was misconfigured |
| `Extract(ExtractError)` | local-LLM extractor faults | only reachable when the graph pipeline is on |
| `Validate(ValidateError)` | input-validation faults | bad bi-temporal bounds, contradictions, missing confirmation token |
| `Retrieve(RetrieveError)` | retrieval-operator faults | an operator in the DSL tree or its backend call failed |
| `Consolidate(ConsolError)` | ACT-R consolidator faults | only reachable when the consolidate pipeline is on |

## Sub-enums

### `StorageError`
| Variant | Notes |
|---|---|
| `Backend(String)` | the Moon backend returned a RESP-level error |
| `NotSupported(&'static str)` | a capability the chosen backend doesn't offer |
| `UnsupportedScheme(String)` | the URL passed to `Lunaris::open` / `lunaris::open` had a scheme other than `moon://` — every other spelling was retired in 0.7.0 |
| `Serde(serde_json::Error)` | a stored value could not be (de)serialized |
| `Io(std::io::Error)` | socket / file IO failure |

### `ExtractError`
`Timeout` (the extractor model didn't answer in budget) · `GrammarReject(String)` (the model's output failed the constrained-decoding grammar) · `Backend(String)`.

### `ValidateError`
| Variant | Notes |
|---|---|
| `Temporal` | `valid_from >= valid_to` on a primitive |
| `Contradiction(String)` | the validator detected a contradiction it routed to the verifier |
| `ConfirmationRequired(String)` | `forget(...).hard()` was called without a confirmation token — do a `dry_run` + `confirm_hard_forget` round-trip first (see [Guides → Forgetting](../guides/forget.md)) |

### `RetrieveError`
`OperatorFailed(String)` (an operator in the retrieval tree errored) · `Backend(String)`.

### `ConsolError`
`ActivationUnderflow` (ACT-R base-level activation went negative — a calibration bug) · `Backend(String)`.

## HTTP mapping (lunaris-server)

| Rust error | HTTP status | `error` code |
|---|---|---|
| `ValidateError::ConfirmationRequired` | `428 Precondition Required` (re-issue the `dry_run` + `confirmation_token` flow) | `confirmation_required` |
| `ValidateError::Temporal`, `ValidateError::Contradiction` (and every other `ValidateError`) | `400 Bad Request` | `validate` |
| `StorageError::NotSupported` (a capability the chosen backend doesn't offer) | `501 Not Implemented` | `not_supported` |
| `StorageError::UnsupportedScheme` | `400 Bad Request` | `unsupported_scheme` |
| request body carrying a `scope` / `tenant` field (forbidden by `#[serde(deny_unknown_fields)]`) | `422 Unprocessable Entity` | (serde rejection) |
| missing / invalid bearer token | `401 Unauthorized` | `unauthorized` |
| valid token without the verb the route requires | `403 Forbidden` | `forbidden` |
| per-tenant rate limit exceeded | `429 Too Many Requests` | (empty body; `Retry-After` header) |
| every other `StorageError`, `RetrieveError`, `ExtractError`, `ConsolError`, unmapped `LunarisError` | `500 Internal Server Error` | `storage` \| `retrieve` \| `extract` \| `consolidate` \| `unknown` |

A handful of statuses are produced by **handler-local validation** rather than
`map_error` — these are not `LunarisError` variants:

| Server cause | HTTP status | `error` code |
|---|---|---|
| `mode: "graph"` on a backend without `graph_native` and with the graph pipeline off (`routes/recall.rs`) | `501 Not Implemented` | `graph_mode_unavailable` |
| malformed `as_of` (not RFC-3339) on `/v1/recall` | `400 Bad Request` | `invalid_request` |
| malformed `confirmation_token` (not a serialized `ForgetReceipt`) on `/v1/forget` | `400 Bad Request` | `invalid_confirmation_token` |
| malformed snapshot Lsn path segment on `/v1/snapshot/{lsn}` | `400 Bad Request` | `invalid_lsn` |
| `{id}` path segment on `/v1/episode/{id}` is not a valid Crockford base-32 ULID | `400 Bad Request` | `invalid_episode_id` |
| `{lsn}` wall_ms on `/v1/snapshot/{lsn}` is strictly greater than the engine's current wall clock | `404 Not Found` | `snapshot_out_of_range` |
| no episode found for `{id}` in the caller's JWT-bound scope on `/v1/episode/{id}` | `404 Not Found` | `episode_not_found` |
| `/metrics` requested while `--metrics-disabled` is set | `404 Not Found` | (plain-text body) |

The auth / rate-limit middleware emits `401` / `403` / `429` *before* the
`LunarisError → status` map runs (`crates/lunaris-server/src/middleware/error.rs::map_error`);
that map only covers business-logic errors. `map_error` also increments
`lunaris_error_total{kind=…}` for every error it handles.

> The wire-level error body shape is specified in [MemoryProtocol 0.1](../protocol/memoryprotocol-0.1.md#error-taxonomy).

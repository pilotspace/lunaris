# Multi-Agent HTTP Contract — Lunaris v0.2

> **Status**: Wave 3H — executable UAT contract for external consumers.
> Authoritative test suite: `crates/lunaris-server/tests/multi_agent_uat.rs`

This document is the **public-facing acceptance contract** for any external
consumer (Helios or otherwise) integrating against `lunaris-server` v0.2. The
five scenarios below map 1:1 to the executable UAT scenarios in
`tests/multi_agent_uat.rs`. An external consumer's CI gate is met when all
five scenarios pass against the v0.2 `lunaris-server` binary.

## Background

Lunaris v0.2 introduces `Scope` — a partition key for multi-agent / multi-tenant
isolation (RFC 0001). Every ingest, recall, and forget operation is bound to the
scope carried by the caller's token claims. Agents cannot read, write, or delete
data across scope boundaries.

> **Not a JWT.** v0 ships **opaque bearer tokens**: the claims (`tenant` =
> partition scope, `scopes` = verb permissions) live in the server-side tokens
> file, never in the token itself. Managed JWT/OIDC issuance is the v1 gate
> `DEPLOY-V1-01`. Docs that say "JWT tenant claim" are using the historical
> wording — see [`SECURITY.md`](../SECURITY.md).

The token file format (`--tokens-file`) is unchanged from v0.1:

```json
{
  "my-bearer-token": {
    "tenant": "agent:helios",
    "scopes": ["ingest", "recall", "forget"]
  }
}
```

The `tenant` value becomes the `Scope` partition key. It must match
`^[A-Za-z0-9_\-:.]{1,128}$`; any other value causes 401 Unauthorized on every
request that uses the token.

---

## Required UAT for External Consumers

### UAT-1: Cross-Scope Ingest + Recall Isolation

Two agents (alpha and beta) must not see each other's data.

**Setup**: Write two tokens to the token file — one for `agent:alpha`, one for
`agent:beta`. Start `lunaris-server` with that file.

**Step 1 — ingest under scope alpha**:

```bash
curl -X POST http://localhost:8080/v1/ingest \
  -H "Authorization: Bearer tok-alpha" \
  -H "Content-Type: application/json" \
  -d '{"source":"agent-alpha:notes","content":"Alice met Bob today"}'
```

Expected response (`200 OK`):

```json
{"lsn":{"wall_ms":1748000000000,"counter":1},"queue_lag_warn":false}
```

**Step 2 — ingest under scope beta**:

```bash
curl -X POST http://localhost:8080/v1/ingest \
  -H "Authorization: Bearer tok-beta" \
  -H "Content-Type: application/json" \
  -d '{"source":"agent-beta:reports","content":"Quarterly revenue grew 12%"}'
```

**Step 3 — recall "Alice" as scope alpha** (must hit):

```bash
curl -X POST http://localhost:8080/v1/recall \
  -H "Authorization: Bearer tok-alpha" \
  -H "Content-Type: application/json" \
  -d '{"query":"Alice","k":5}'
```

Expected: `200 OK`, non-empty JSON array, first hit's `text` contains `"Alice"`.

**Step 4 — recall "Alice" as scope beta** (must return empty — no cross-scope leak):

```bash
curl -X POST http://localhost:8080/v1/recall \
  -H "Authorization: Bearer tok-beta" \
  -H "Content-Type: application/json" \
  -d '{"query":"Alice","k":5}'
```

Expected: `200 OK`, **empty JSON array `[]`**. Scope beta cannot see scope
alpha's data.

**Step 5 — recall "revenue" as scope beta** (must hit its own data):

```bash
curl -X POST http://localhost:8080/v1/recall \
  -H "Authorization: Bearer tok-beta" \
  -H "Content-Type: application/json" \
  -d '{"query":"revenue","k":5}'
```

Expected: `200 OK`, non-empty array, hit `text` contains `"revenue"`.

---

### UAT-2: Malformed Scope → 401

Tokens whose `tenant` field violates the scope validation regex must be rejected
with `401 Unauthorized` before any handler logic runs.

**Case A — empty tenant**:

```bash
# Token file: {"bad-tok": {"tenant": "", "scopes": ["ingest"]}}
curl -X POST http://localhost:8080/v1/ingest \
  -H "Authorization: Bearer bad-tok" \
  -H "Content-Type: application/json" \
  -d '{"source":"s","content":"hello"}'
```

Expected: `401 Unauthorized`

```json
{"error":"unauthorized","message":"token tenant is not a valid scope identifier"}
```

**Case B — tenant longer than 128 characters** (use any 129-char string):

Expected: `401 Unauthorized` (same body as above)

**Case C — tenant contains invalid characters** (`%`, space, backslash):

```bash
# Tenant "bad%scope" or "bad scope" or "bad\scope"
```

Expected: `401 Unauthorized`

The validation regex is `^[A-Za-z0-9_\-:.]{1,128}$`. Any character outside
this set causes rejection.

---

### UAT-3: Request Body Cannot Override Scope

The `POST /v1/ingest` body MUST NOT contain a `"scope"` or `"tenant"` key.
`IngestBody` uses `serde(deny_unknown_fields)` — unknown fields cause a
`422 Unprocessable Entity` before the handler runs.

**Case A — body contains `"scope"`**:

```bash
curl -X POST http://localhost:8080/v1/ingest \
  -H "Authorization: Bearer tok-alpha" \
  -H "Content-Type: application/json" \
  -d '{"source":"evil","content":"injecting scope","scope":"victim-scope"}'
```

Expected: `422 Unprocessable Entity`

```json
{"error":"unprocessable_entity","message":"Failed to parse the request body as JSON: unknown field `scope` ..."}
```

**Case B — body contains `"tenant"`**:

```bash
curl -X POST http://localhost:8080/v1/ingest \
  -H "Authorization: Bearer tok-alpha" \
  -H "Content-Type: application/json" \
  -d '{"source":"evil","content":"injecting tenant","tenant":"victim-tenant"}'
```

Expected: `422 Unprocessable Entity`

**Invariant**: After either 422 response, the storage must contain zero
episodes for scope alpha. No partial writes occur.

---

### UAT-4: Forget Honors Scope

A cross-scope forget attempt cannot delete data in another scope's partition.

**Step 1 — ingest two episodes under scope alpha**:

```bash
curl -X POST http://localhost:8080/v1/ingest \
  -H "Authorization: Bearer tok-alpha" \
  -H "Content-Type: application/json" \
  -d '{"source":"src:ep1","content":"Episode 1 content"}'
# Note the `lsn` field but also capture the episode id from a recall if needed
```

**Step 2 — dry-run forget from scope beta targeting scope alpha's episode id**:

```bash
# Use an episode id that belongs to scope alpha
curl -X POST http://localhost:8080/v1/forget \
  -H "Authorization: Bearer tok-beta" \
  -H "Content-Type: application/json" \
  -d '{"target":{"Id":"01HX0000000000000000000000"},"dry_run":true}'
```

Expected: `200 OK` with receipt showing zero rows affected:

```json
{
  "target": {"Id": "01HX0000000000000000000000"},
  "indices_affected": ["Kv"],
  "rows_written": 0,
  "rows_deleted": 0,
  "audit_lsn": {"wall_ms": 0, "counter": 0},
  "preview": true
}
```

`rows_written == 0` and `rows_deleted == 0` — the cross-scope forget found
no rows in the beta partition and had no effect on alpha's data.

**Step 3 — verify scope alpha's data is intact**:

```bash
curl -X POST http://localhost:8080/v1/recall \
  -H "Authorization: Bearer tok-alpha" \
  -H "Content-Type: application/json" \
  -d '{"query":"Episode","k":5}'
```

Expected: `200 OK`, non-empty array — scope alpha's episodes are untouched.

> **Note for v0.3 adopters**: The forget handler is being migrated to full
> scope-binding (`ScopedLunaris::forget`). Once that lands, a cross-scope
> forget will return `403 Forbidden` or `404 Not Found` instead of `200`
> with zero rows. Consumers MUST be prepared to handle both status codes.

---

### UAT-5: Concurrent Multi-Agent Traffic Smoke

10 agents each perform 3 ingest + 3 recall operations concurrently (60 total
HTTP calls). All calls must succeed with `200 OK` and each agent must only see
its own data.

**Setup**: 10 tokens with tenants `agent:0` through `agent:9`.

**Validation script** (pseudocode):

```bash
for i in 0..9; do
  for j in 0..2; do
    # Ingest
    curl -sf -X POST http://localhost:8080/v1/ingest \
      -H "Authorization: Bearer tok-agent-$i" \
      -H "Content-Type: application/json" \
      -d "{\"source\":\"agent:$i:src\",\"content\":\"observation $j from agent $i\"}" | jq -e '.lsn'

    # Recall
    curl -sf -X POST http://localhost:8080/v1/recall \
      -H "Authorization: Bearer tok-agent-$i" \
      -H "Content-Type: application/json" \
      -d "{\"query\":\"agent $i\",\"k\":3}" | jq -e '. | length >= 0'
  done &
done
wait
```

Expected: All 60 calls exit `0` (jq assertions pass). No agent sees data
from another agent's scope in the recall results.

---

## Token File Reference

```json
{
  "<bearer-token-string>": {
    "tenant": "<scope-id>",
    "scopes": ["ingest", "recall", "forget"]
  }
}
```

| Field    | Type            | Constraint                                    |
|----------|-----------------|-----------------------------------------------|
| `tenant` | string          | `^[A-Za-z0-9_\-:.]{1,128}$` — validated at auth boundary |
| `scopes` | array of string | Subset of `["ingest", "recall", "forget"]`   |

A token lacking the required scope for a route receives `403 Forbidden`:

```json
{"error":"forbidden","message":"token lacks required scope `recall`"}
```

---

## v0.1 → v0.2 Breaking Changes (Server Layer)

| Change | v0.1 | v0.2 |
|---|---|---|
| `POST /v1/ingest` body type | Raw `Episode` JSON (had `scope` field) | `IngestBody` — no `scope` field; `deny_unknown_fields` |
| Auth middleware | `tenant: String` attached to request | `scope: Scope` — validated at parse time; invalid → 401 |
| Storage partitioning | Global namespace | Per-scope via `Scope` newtype passed to every `StoragePort` call |
| Cross-scope recall | Could leak if storage-level isolation was misconfigured | Enforced at `vector_search` + `read_as_of` call sites |

Full migration guide: [`docs/migration/0.1-to-0.2.md`](migration/0.1-to-0.2.md)

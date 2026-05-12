# Conformance

> Adapted from `docs/protocol/conformance.md` (kept in the repo as the
> standalone version).

The `lunaris-conformance` crate (`crates/lunaris-conformance/`) ships three
re-usable suites that any backend or protocol implementation can certify
against:

1. **Storage suite** — parameterized over `Arc<dyn StoragePort>`. Tests every
   method on the trait surface (`atomic_write`, `vector_search`,
   `graph_traverse`, `scan_range`, `read_as_of`, `publish`/`subscribe`,
   `capabilities`). Plan 05-02 STORE-05.
2. **Protocol suite** — parameterized over `(reqwest::Client, base_url,
   token)`. Tests the four [MemoryProtocol](./memoryprotocol-0.1.md) verbs +
   SSE + auth + rate limit + retrieval modes. Plan 05-03 PROTO-06.
3. **AS_OF parity** — dual-backend differential test that asserts `recall`
   against Moon and Postgres returns identical hits + ordering for the same
   input. Plan 05-02 STORE-07.

The reference implementation under test is `lunaris-server`
(`crates/lunaris-server/`, axum 0.8 binary). The harness is library-shaped
(CONTEXT.md D-11) so any third-party `StoragePort` impl or HTTP server can
wire to it without duplicating test code.

## Suites

| Suite        | Function                                                       | Tests | What it covers                                                                                  |
|--------------|---------------------------------------------------------------|-------|-------------------------------------------------------------------------------------------------|
| Storage      | `lunaris_conformance::run_full_storage_suite(storage)`        | 8     | atomic_write, vector_search, graph_traverse (gated), scan_range, read_as_of, publish/subscribe, capabilities |
| Protocol     | `lunaris_conformance::run_full_protocol_suite(client, url, t)`| 10    | POST /v1/ingest, POST /v1/recall (default + SSE + graph mode), POST /v1/forget (id + two-step hard), GET /v1/snapshot/{lsn}, auth (401 + 403), rate-limit (429 + Retry-After) |
| AS_OF parity | `lunaris_conformance::storage::as_of_parity::run(moon, pg)`   | 1     | STORE-07 — Moon vs Postgres identical hits; capability-gated `Divergence` for rerank-score precision |

The Plan 04-03 chaos / crash-recovery property test (`tests/crash_recovery.rs`)
ships under the same crate gated on the `chaos-it` Cargo feature.

## Running locally

### Storage suite (per backend)

```bash
# Moon
MOON_URL=moon://localhost:6390 \
  cargo test -p lunaris-conformance --test run_storage_moon -- --nocapture

# Postgres
PG_URL=postgres://postgres:lunaris@localhost/lunaris \
  cargo test -p lunaris-conformance --test run_storage_postgres -- --nocapture

# AS_OF parity (requires BOTH)
MOON_URL=moon://localhost:6390 \
PG_URL=postgres://postgres:lunaris@localhost/lunaris \
  cargo test -p lunaris-conformance --test run_as_of_parity -- --nocapture
```

When env vars are unset → SKIPS cleanly (exit 0). When the TCP probe fails →
SKIPS with diagnostic.

### Protocol suite (against `lunaris-server`)

```bash
# 1. Build the binary so the subprocess runner can find it.
cargo build -p lunaris-server  # → target/debug/lunaris-server

# 2. Run the suite (picks MOON_URL first, falls back to PG_URL).
MOON_URL=moon://localhost:6390 \
  cargo test -p lunaris-conformance \
    --test run_protocol_lunaris_server -- --nocapture
```

The runner spawns `lunaris-server --bind 127.0.0.1:0 --rate-burst 10
--rate-per-second 5` as a subprocess, parses `LISTENING_ON <addr>` from its
stderr (Plan 05-01 `main.rs:53-57` contract), then runs the protocol suite
against the ephemeral port. Cleanup (kill child + remove temp tokens-file)
happens via RAII Drop guards regardless of test outcome.

### Chaos / crash-recovery (Unix only, gated)

```bash
MOON_URL=moon://localhost:6390 \
PG_URL=postgres://postgres:lunaris@localhost/lunaris \
  cargo test -p lunaris-conformance \
    --features chaos-it --test crash_recovery -- --nocapture
```

See [Durability & Recovery](../operations/durability.md) for the full
crash-recovery story.

## Certifying a third-party implementation

### Third-party `StoragePort` impl

Add `lunaris-conformance` as a dev-dependency:

```toml
# Cargo.toml
[dev-dependencies]
lunaris-conformance = { path = "../lunaris/crates/lunaris-conformance" }
lunaris-core        = { path = "../lunaris/crates/lunaris-core" }
tokio               = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Write a thin entry test:

```rust
#[tokio::test]
async fn my_storage_conformance() -> anyhow::Result<()> {
    let storage = MyStorage::open("my://localhost:1234").await?;
    lunaris_conformance::run_full_storage_suite(std::sync::Arc::new(storage)).await
}
```

Conformant if and only if exit code is `0`.

### Third-party MemoryProtocol server

Two paths depending on whether you can spawn the third-party server from a
test or only point at an already-running endpoint.

**Path A — already-running server:**

```rust
#[tokio::test]
async fn my_server_protocol_conformance() -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let base = url::Url::parse("http://localhost:7000")?;
    lunaris_conformance::run_full_protocol_suite(client, base, "tok-test".to_string()).await
}
```

The server MUST honor the [MemoryProtocol 0.1](./memoryprotocol-0.1.md)
contract verbatim: in particular, the test-only token map MUST contain
`tok-test` (full scopes) and `tok-ingest` (ingest-only scope) for the auth +
scope-isolation tests to pass.

**Path B — spawn from the test:**

Mirror `crates/lunaris-conformance/tests/run_protocol_lunaris_server.rs`:
spawn the server binary, parse its bound address from stdout/stderr, run the
suite, kill the child via RAII Drop guard.

## What "conformant" means

A conformant implementation:

1. Returns the exact HTTP status codes and response shapes documented in
   [MemoryProtocol 0.1](./memoryprotocol-0.1.md) — every cell in the
   [§Error taxonomy](./memoryprotocol-0.1.md#error-taxonomy) table is
   testable.
2. Honors every `StorageCapabilities` field accurately. A backend that
   reports `graph_native: true` MUST implement Cypher subset queries; a
   backend that reports `queue_native: true` MUST implement `publish` /
   `subscribe` round-trip semantics.
3. Surfaces `Hit::degraded = true` when the verifier-queue depth check fires
   (when applicable to the deployment).
4. Issues exactly one `atomic_write` per `ingest` / `forget` call
   (single-write invariant from Plans 04-04 / 04-05; INGEST-04).
5. Publishes one `AuditEvent` to `__lunaris_audit__` on every successful
   `forget` (best-effort, fire-and-forget per Plan 04-05 OPS-04).
6. Honors the D-21 two-step hard-delete rail: `hard: true` without
   `confirmation_token` MUST return `428 Precondition Required`.
7. Returns `429 Too Many Requests` with a `Retry-After` header on rate-limit
   exhaustion.

## Environment variables

| Variable                     | Purpose                                                   | Required for                                  |
|------------------------------|-----------------------------------------------------------|-----------------------------------------------|
| `MOON_URL`                   | Moon backend connect URL (e.g., `moon://localhost:6390`)  | storage suite (Moon), AS_OF parity, protocol  |
| `PG_URL`                     | Postgres backend connect URL                              | storage suite (Postgres), AS_OF parity, protocol |
| `CARGO_TARGET_DIR`           | Override target dir for binary discovery                  | protocol suite when out-of-tree builds used   |
| `CARGO_BIN_EXE_lunaris-server` | Pre-resolved binary path (forward-compat)               | protocol suite (rare; harness falls back)     |

When `MOON_URL` AND `PG_URL` are both unset, every test SKIPS cleanly —
`cargo test --workspace` stays green on a fresh checkout without backends.

## CI integration

The workspace's GitHub Actions workflow (`.github/workflows/eval-gauntlet.yml`
— landed by Plan 05-06) provisions Moon + Postgres via Docker services and
runs the full conformance suite on every push + pull request. Suggested
invocation block:

```yaml
- run: cargo build -p lunaris-server                                          # protocol-suite prereq
- run: cargo test -p lunaris-conformance --test run_storage_moon
- run: cargo test -p lunaris-conformance --test run_storage_postgres
- run: cargo test -p lunaris-conformance --test run_as_of_parity
- run: cargo test -p lunaris-conformance --test run_protocol_lunaris_server
- run: cargo test -p lunaris-conformance --features chaos-it --test crash_recovery
```

All 5 invocations exit `0` when env vars unset (clean skip) OR backends
reachable + suite passes. They exit `1` when backends are reachable AND suite
assertions fire — exactly the gate behavior CI wants.

## Glossary

- **`lunaris_conformance::run_full_storage_suite`** — entry to the storage suite (Plan 05-02 STORE-05).
- **`lunaris_conformance::run_full_protocol_suite`** — entry to the protocol suite (Plan 05-03 PROTO-06).
- **`lunaris_conformance::storage::as_of_parity::run`** — entry to the AS_OF parity test (Plan 05-02 STORE-07).
- **`Divergence`** — typed enum carrying every observed mismatch in AS_OF parity (HitCount / HitOrdering / ScoreEpsilon); ScoreEpsilon is suppressed when backends disagree on `rerank_native`.
- **probe_backend** — TCP-probe + 1s timeout helper (verbatim from Plan 04-03 `crash_recovery.rs::probe_backend` — W-3 + W-7 fixes).
- **B-7 stub** — forward-compat convention: ship public signatures with `Ok(())` bodies in scaffold tasks so dependent plans can wire to the surface before bodies land. Used by Plan 05-02 to ship the protocol module before Plan 05-03 filled it in.

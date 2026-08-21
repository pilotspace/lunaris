# Forgetting (GDPR / audit)

**Reach for this chapter when a primitive must stop being visible to future
queries** — GDPR right-to-be-forgotten, retention windows, session cleanup.
Three variants, one entry point (`Lunaris::forget`,
`crates/lunaris/src/forget.rs:208`), one `__lunaris_audit__` event per
successful call.

> ## v0.2.x headline gotcha — read this first
>
> **`Lunaris::forget` is hard-coded to `Scope::dev()` internally** for its
> `atomic_write` / `read_as_of` / `scan_range` calls
> (`crates/lunaris/src/forget.rs:300-303`). Under **any** non-`_dev_` scope it
> silently returns `rows_written = 0, rows_deleted = 0` — the Moon SCAN
> prefix filters everything out. It emits a `tracing::warn!` on every
> call so the line above your `forget(...)` says so. The real per-scope routing
> — `ScopedLunaris::forget(target)` with a `403`/`404` cross-scope contract —
> is a **v0.3 deliverable**. See RFC 0001 §11.6 and `CHANGELOG.md`
> "Known issues". Until then, forget works against the `_dev_` scope only.

## The three targets

```rust
pub enum ForgetTarget {
    Id(Ulid),               // OPS-01 — single-primitive purge across KV + vector + graph
    Scope(ScopeSpec),       // OPS-02 — prefix / metadata / episode-id predicate
    Before(Hlc),            // OPS-03 — AS_OF cutoff
}

pub enum ScopeSpec {
    BySource(String),           // prefix match on episode.source
    ByMetadata(String, String), // exact match on metadata[key] == value
    ByEpisode(Ulid),            // exact match on episode.id
}
```

(`ForgetTarget` and `ScopeSpec` are `#[non_exhaustive]`,
`crates/lunaris/src/forget.rs:49-73`.)

## Soft vs hard delete

| Request | What it does | `atomic_write` calls |
|---|---|---|
| `target` (bare) | **Soft delete.** Stamps the MVCC `bt.sys_to` on each match — prior reads via `as_of` still see the data. | 1 (`KvPut` of sys-stamped payloads); 0 if no match |
| `target.dry_run()` | **Preview.** No write at all. Returns a receipt with `preview = true`, `rows_written = 0`. The audit event still publishes (ops gets a trail of what almost happened). | 0 |
| `target.hard()` (no token) | `Err(LunarisError::Validate(ValidateError::ConfirmationRequired(_)))` — not a panic. | 0 |
| `target.hard().with_token(t)` | **Hard delete.** Irreversible `KvDelete` fan-out. Requires a `ForgetConfirmation` minted from a prior `dry_run` receipt (the D-21 two-step safety rail). | 1 (`KvDelete` ops) |

D-19 single-call invariant: every successful `forget` issues **at most one**
`atomic_write` (zero for dry-run or no-match).

Soft delete writes the `sys_to` *inside the payload bytes* — backends derive
persisted bi-temporal from the payload, so a typed-only mutation would be
silently lost. `build_soft_delete_op` patches both the in-memory `BiTemporal`
and `payload["bt"]["sys"][1]` (`crates/lunaris/src/forget.rs`), the same
cross-plan contract the verifier's `apply_supersede` uses. This is what makes
"forget" compatible with bi-temporal MVCC: the data is hidden from
default-time queries but a `recall().as_of(t_before_forget)` still returns it.

## Code

```rust
use lunaris::{ForgetTarget, ScopeSpec};

// Soft delete a session prefix.
let target = ForgetTarget::Scope(ScopeSpec::BySource("helios:fs/session-42/".into()));
let receipt = lunaris.forget(target.clone()).await?;
assert!(!receipt.preview);
// receipt.rows_written == number of MVCC rows stamped (0 under a non-_dev_ scope!)

// Dry-run preview.
let preview = lunaris.forget(target.clone().dry_run()).await?;
assert!(preview.preview);

// Hard delete — two steps.
let token = lunaris.confirm_hard_forget(preview).await?;
let hard  = lunaris.forget(target.hard().with_token(token)).await?;
assert!(!hard.preview);
assert_eq!(hard.rows_written, 0);  // hard delete writes zero MVCC rows
// hard.rows_deleted == one KvDelete per match
```

`confirm_hard_forget` only accepts a `preview: true` receipt — replaying a
non-preview receipt returns the same `ConfirmationRequired` error
(`crates/lunaris/src/forget.rs:333`).

## The receipt

```rust
pub struct ForgetReceipt {
    pub target: ForgetTarget,
    pub indices_affected: Vec<IndexKind>,  // Kv | Vector | Graph
    pub rows_written: u64,                 // soft-delete MVCC writes; 0 for hard / dry-run
    pub rows_deleted: u64,                 // irreversible deletes; 0 for soft / dry-run
    pub audit_lsn: Lsn,                    // __lunaris_audit__ publish offset
    pub preview: bool,                     // true iff dry_run (no atomic_write happened)
}
```

`ForgetConfirmation` carries `for_audit_lsn` and cannot be constructed by the
caller — only returned from `confirm_hard_forget`.

## GDPR / audit notes

- **One audit event per successful call** — soft, hard, *and* dry-run. The
  audit publish lands even on dry-run so operators have a complete trail. The
  receipt's `audit_lsn` is the `__lunaris_audit__` offset.
- **Use `dry_run` first** for any destructive run — it's also the only way to
  mint the hard-delete confirmation token.
- **Hard delete is irreversible.** Soft delete is the GDPR-friendly default:
  the data leaves default-time queries immediately, and MVCC retention keeps
  it auditable until your retention policy hard-deletes it.

## HTTP

Over the wire (`POST /v1/forget`), the request DTO carries
`#[serde(deny_unknown_fields)]` — a `scope` / `tenant` field is a `422`. Hard
delete is two requests: `dry_run: true`, read the `audit_lsn` out of the
receipt, then repeat with `hard: true` and a `confirmation_token` formed from
the prior audit LSN. See the [MemoryProtocol spec](../protocol/memoryprotocol-0.1.md).

## See also

- [Cookbook → Helios Scratchpad](../cookbook/helios-scratchpad.md) — uses a
  `BySource` prefix soft delete (`pad.forget()`).
- [Durability & Recovery](../operations/durability.md) — bi-temporal MVCC and
  what `as_of` recovers after a forget.
- [Multi-Agent & Scope](./multi-agent.md) — why `forget` is `_dev_`-only in
  v0.2.x.

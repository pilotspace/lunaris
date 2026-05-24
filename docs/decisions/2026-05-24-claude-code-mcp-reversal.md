# ADR: Claude Code MCP Surface Shape — Option A (stdio) Adopted, Option C Rejected

**Date:** 2026-05-24
**Status:** Accepted
**Author:** Tin Dang
**Supersedes:** Out-of-scope row in `docs/helios-integration.md:13`

---

## Context

`docs/helios-integration.md` line 13 contains:

> See also the Out-of-Scope row in `PROJECT.md` ("Claude Code FS adapter
> shape (CAS, mtime, prefix_scan_meta_only) inside Lunaris") — Helios
> FS-tool ergonomics live in the Helios repo, not this one.

`PROJECT.md` does not exist in this repository. The pointer was written
during an early planning pass before `PROJECT.md` was formalised. The
out-of-scope row text is reproduced above from the `helios-integration.md`
reference; this ADR is the canonical resolution of that open question.

Three options were on the table when the Claude Code integration wave began:

**Option A — Standalone `lunaris-mcp` binary (stdio)**
Ship a dedicated `crates/lunaris-mcp` binary. Transport: stdio (newline-
delimited JSON-RPC). Auth: none (stdio is process-bound by the MCP client).
Scope is bound at startup from CLI/env; no wire field can override it.

**Option B — `lunaris-server` SSE endpoint + Bearer auth**
Add an `mcp` feature flag to `lunaris-server` that exposes an SSE-over-HTTP
MCP endpoint protected by Bearer tokens. Intended for multi-user server
deployments.

**Option C — MCP as a feature flag on `lunaris-server` (stdio shim)**
Wire a stdio-to-HTTP shim inside `lunaris-server`. Evaluate whether the
Claude Code FS adapter shape (CAS, mtime, `prefix_scan_meta_only`) should be
modelled as a first-class Lunaris primitive rather than a Helios-side concern.

---

## Decision

**Option A is adopted for Wave A.** Option B is deferred. Option C is
rejected.

---

## Rationale

### Why Option A

- **Cold-start budget.** The Wave 3.2 cold-start budget gate requires
  `tools/list` to return in under 500 ms. A standalone stdio binary with
  lazy GGUF staging (models loaded on first `memory.recall`, not at startup)
  satisfies this without HTTP stack overhead.
- **Scope isolation is cheaper at the binary boundary.** `lunaris-mcp`
  accepts `--scope` / `LUNARIS_MCP_SCOPE` once at startup. The scope is
  wired into `AppState` and physically cannot be overridden by wire payloads
  (`#[serde(deny_unknown_fields)]` on all DTOs). An HTTP server approach
  requires JWT-bound scope extraction on every request — correct but more
  surface area for RC bugs.
- **Deployment simplicity.** `cargo install lunaris-mcp` + two config lines
  is the full install story. No port exposure, no TLS, no token rotation for
  the common single-developer case.
- **Internal-first audience.** The v0 audience is internal agent platforms
  (Helios, Claude Code). stdio is sufficient and safe: MCP clients sandbox
  the child process.

### Why Option B is deferred, not rejected

Option B is the correct answer for multi-user deployments (team servers,
hosted SaaS). It requires SSE transport support in `rmcp`, a Bearer token
issuance path, and a scope-per-connection model. None of that is needed for
Wave A. It is deferred to Wave B/C, not cancelled.

### Why Option C is rejected

Option C conflated two unrelated concerns:

1. Whether MCP should be a mode of `lunaris-server` (an HTTP server) — this
   is Option B, not C.
2. Whether the "Claude Code FS adapter shape (CAS, mtime,
   `prefix_scan_meta_only`)" should be a first-class Lunaris primitive.

On point 2: Helios FS-tool ergonomics belong in the Helios repo. The boundary
stated in `docs/helios-integration.md` §"Boundary statement" is correct and
unchanged: Lunaris exports `HeliosScratchpad`; it does not know Helios exists
at the type level. Introducing CAS, mtime, or `prefix_scan_meta_only` as
Lunaris primitives would violate this boundary — Lunaris would become shaped
by one downstream consumer's UX decisions. The `lunaris-mcp` wire surface
(`memory.ingest`, `memory.recall`, `memory.forget`, `memory.list_scopes`) is
intentionally generic; Claude Code FS ergonomics are a Claude Code concern.

---

## Constraints Locked by This Decision

- The `lunaris-mcp` crate MUST remain transport-agnostic at the handler
  layer. Handlers receive `&AppState` + typed params; transport wiring lives
  in `main.rs` only. This preserves the option to add HTTP/SSE in Wave B
  without rewriting handlers.
- CAS, mtime, and `prefix_scan_meta_only` MUST NOT be added as fields to
  any `lunaris-core` type. If Helios needs them, Helios encodes them in
  `metadata` or in its own recipe layer.
- The `#[serde(deny_unknown_fields)]` invariant on all MCP wire DTOs is
  non-negotiable. It is the type-level enforcement that makes scope isolation
  hold across transport boundaries.

---

## What Stays Out of Scope (Unchanged from the Original Row)

The original `PROJECT.md` out-of-scope row covered:

> Claude Code FS adapter shape (CAS, mtime, prefix_scan_meta_only) inside Lunaris

This stays out of scope. The `lunaris-mcp` surface does not model
filesystem operations — episodes have `source`, `content`, `t_ref`, and
`metadata`. Filesystem semantics (content-addressable storage, modification
timestamps, prefix-scan with metadata-only projection) are Helios-side
concerns and remain so.

---

## References

- `docs/helios-integration.md:13` — original out-of-scope pointer
- `crates/lunaris-mcp/src/main.rs` — tool registration and transport wiring
- `crates/lunaris-mcp/src/state.rs` — scope bootstrap (`AppState::bootstrap`)
- `docs/integration/claude-code.md` — Wave A integration guide
- `docs/integration/codex.md` — Codex CLI parity guide

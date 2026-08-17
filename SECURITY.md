# Security Policy

## Supported versions

Lunaris is pre-1.0; only the newest release line receives security fixes.

| Version | Supported |
|---|---|
| 0.6.x (latest patch) | ✅ security fixes |
| main (0.7.0-dev) | ✅ fixes land here first |
| ≤ 0.5.x | ❌ upgrade — see `docs/migration/` |

## Reporting a vulnerability

Please report vulnerabilities **privately** — do not open a public issue.

- Email: **security@lunaris.dev**
  <!-- PLACEHOLDER: this address requires owner confirmation that the
       lunaris.dev MX/mailbox exists before this file is advertised
       externally. Until then, use GitHub private vulnerability reporting. -->
- Or use GitHub's private vulnerability reporting on
  `github.com/pilotspace/lunaris` (Security → Report a vulnerability).

You should receive an acknowledgement within 3 business days. Please include
a reproduction, the affected version/commit, and the deployment shape
(HTTP server / MCP stdio / SDK embedding).

## v0 security stance — read before deploying

Lunaris' v0 threat model assumes the server runs **inside a trusted network
perimeter** with hardening delegated to your reverse proxy and network
layer. This is a documented design decision, not an oversight
(`DEPLOY-V1-01`, `docs/book/src/operations/server.md` "Deployment notes").

What the code actually does today:

- **Authentication = opaque bearer tokens from a boot-loaded JSON file.**
  `lunaris-server` loads `LUNARIS_TOKENS_FILE` once at startup
  (`crates/lunaris-server/src/lib.rs:111`, `load_tokens` at `lib.rs:337`)
  into an in-memory map; the auth middleware resolves
  `Authorization: Bearer <token>` by map lookup
  (`crates/lunaris-server/src/middleware/auth.rs:61-64`). Tokens are stored
  in **plaintext** on disk, there is no hashing, no expiry, and no runtime
  rotation endpoint — rotating a token means editing the file and
  restarting the process. Protect the tokens file with filesystem
  permissions and treat it as a secret.
  There are **no JWTs** in v0. The book occasionally says "JWT tenant
  claim" for historical reasons; the wire credential is an opaque token
  whose claims (`tenant` = partition scope, `scopes` = verb permissions)
  live in the server-side tokens file, never in the token itself.
  Managed JWT/OIDC issuance is the v1 gate `DEPLOY-V1-01`.
- **Transport = plain HTTP.** No TLS termination in-process. Terminate TLS
  (and do OAuth2/OIDC, IP allow-listing, request signing — anything beyond
  bearer auth) at a reverse proxy in front of the listener.
- **`/metrics` is unauthenticated by design** (accepted as T-05-05-05;
  `crates/lunaris-server/src/routes/metrics.rs:12-17`), following standard
  Prometheus convention. Metric labels include tenant/scope names, so an
  exposed `/metrics` leaks your tenant roster. Restrict it with a network
  ACL or reverse-proxy auth, or disable it with `--metrics-disabled`.
- **CORS defaults to `*`** (`crates/lunaris-server/src/config.rs:65-67`).
  Fine for non-browser API clients; if browsers ever talk to your
  deployment, set `LUNARIS_CORS_ORIGINS` to an explicit origin list.
- **Tenant isolation is server-side.** The partition scope comes only from
  the token's server-side `tenant` entry; request bodies cannot override it
  (every public DTO carries `#[serde(deny_unknown_fields)]`).

The operator-facing hardening checklist lives in the book:
`docs/book/src/operations/security.md`.

## Scope

In scope: the crates in this workspace (`crates/*`), the published SDK
packages, the `lunaris-mcp` server, hooks, and the deploy manifests under
`deploy/`. Out of scope: the Moon substrate itself (report via the Moon
repo), and vulnerabilities requiring a hostile process already inside the
trusted perimeter with access to the tokens file.

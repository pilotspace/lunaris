# Security & Hardening

Lunaris' v0 security model is deliberately thin in-process and delegates
hardening to the layer in front of it: the server assumes a **trusted
network perimeter**, with TLS, OAuth2/OIDC, and IP filtering done at a
reverse proxy (`DEPLOY-V1-01` — see
[Running the HTTP Server → Deployment notes](./server.md#deployment-notes)).
This page is the operator checklist for deploying inside that model.
Vulnerability reporting and the full stance rationale live in the repo-root
[`SECURITY.md`](https://github.com/pilotspace/lunaris/blob/main/SECURITY.md).

## What v0 auth actually is

- **Opaque bearer tokens, boot-loaded.** `lunaris-server` reads
  `LUNARIS_TOKENS_FILE` (a JSON map) once at startup
  (`crates/lunaris-server/src/lib.rs`, `load_tokens`) and resolves
  `Authorization: Bearer <token>` by in-memory map lookup
  (`middleware/auth.rs`). There are **no JWTs** — the "claims" (`tenant`
  partition scope, `scopes` verb permissions) live server-side in the
  tokens file, never inside the token string.
- **No expiry, no runtime rotation.** Rotating a token = edit the tokens
  file, restart the server. Plan rotation as a rolling restart.
- **Plaintext at rest.** The tokens file is a secret; the server never
  hashes it. `chmod 600` and owner it to the service user.
- **A missing/corrupt tokens file does not stop the boot** — the server
  starts with an empty map (every request 401s) so `/healthz` still
  answers. Watch the boot warning log.
- **Tenant isolation is wire-proof.** The partition scope comes only from
  the token's server-side `tenant` entry; every public DTO carries
  `#[serde(deny_unknown_fields)]`, so a request body smuggling a
  `scope`/`tenant` field is rejected with 422.

## Hardening checklist

Before exposing a deployment to anything beyond localhost:

- [ ] **Reverse proxy in front, TLS terminated there.** v0 is plain HTTP
      (`DEPLOY-V1-01`). Do OIDC/OAuth2, request size limits, and IP
      allow-listing at the proxy too.
- [ ] **Restrict `/metrics`.** It is unauthenticated by design (accepted
      as T-05-05-05, `routes/metrics.rs`; standard Prometheus convention)
      and its labels **include your tenant roster**. Network ACL,
      proxy-side auth, or `--metrics-disabled`.
- [ ] **Set `LUNARIS_CORS_ORIGINS`.** The default is `*`
      (`crates/lunaris-server/src/config.rs`). Harmless for non-browser
      clients; set an explicit origin list the moment a browser is in the
      picture.
- [ ] **Lock down the tokens file.** `chmod 600`, service-user owned,
      excluded from backups that leave the trust boundary.
- [ ] **Keep Moon off the public network.** The server↔Moon link
      (`moon://host:port`) is unauthenticated RESP inside your perimeter;
      firewall Moon's port to the server hosts, and treat Moon's admin
      port (`--admin-port`, serves its own unauthenticated `/metrics`)
      the same way.
- [ ] **One token per client, least verbs.** `scopes` grants
      `ingest`/`recall`/`forget` per token — a recall-only consumer
      should hold a recall-only token (missing verb → 403).
- [ ] **Rate limits are per-tenant, defaults 60 rps / burst 120**
      (`LUNARIS_RATE_PER_SECOND` / `LUNARIS_RATE_BURST`) — size them to
      your clients before load-testing convinces you the server is broken.
- [ ] **Logs:** production JSON logging (`LUNARIS_ENV=production`)
      includes correlation IDs; ship them somewhere with retention if you
      need an audit trail. There is no in-process audit log beyond
      tracing output.

## What is deferred, on record

| Deferred | Where that's recorded |
|---|---|
| TLS in-process, OAuth2/OIDC, managed JWT issuance | `DEPLOY-V1-01`, [server.md → Deployment notes](./server.md#deployment-notes) |
| `/metrics` auth | T-05-05-05 accept (comment in `routes/metrics.rs`) |
| Token hashing / runtime rotation endpoint | v0 tokens-file design (D-07) — revisit with `DEPLOY-V1-01` |
| OTLP trace export | ADR `docs/decisions/2026-08-17-otlp-post-ga.md` (post-GA) |

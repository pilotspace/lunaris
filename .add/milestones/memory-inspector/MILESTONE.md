# MILESTONE: Memory Inspector — Phase 1

goal: A reviewer can open a local browser, pick a scope, and browse and understand every captured memory — full content, provenance, and the entity graph — without writing a query.
rationale: sub-milestone — a self-contained read-only review surface over the memory that ingest/recall already produce; no new write path, no schema change. Phase 1 is Moon-native; the bi-temporal timeline is a deliberate Phase-2 split gated on a later history-source decision.
stage: mvp · status: active · created: 2026-06-16

> SDD living doc for this milestone. Keep it THIN: breadth, shared decisions, and
> exit criteria only — per-task detail lives in each `.add/tasks/<slug>/TASK.md`,
> written just-in-time. Update this doc whenever a task reveals a milestone gap.

## Scope
In:  read-only HTTP surface in `lunaris-server` (`GET /v1/scopes`, `GET /v1/browse/{kind}`,
     `GET /v1/detail/{kind}/{id}`, `GET /v1/graph?root=&depth=`) + a single-file, server-served
     vanilla SPA at `GET /` (scope picker · paginated browse table · lineage drawer · entity-graph
     canvas · disabled Phase-2 timeline affordance). All `/v1` routes JWT-scope-bound; the shell is
     public but secret-free.
Out: bi-temporal timeline (as-of / superseded / forgotten history) · `GET /v1/history/{kind}/{id}` ·
     `superseded_by` payload writes — all Phase 2, gated on the history-source decision
     (audit-stream vs Moon STORE-07 fix vs Postgres). No write path of any kind from the UI.

## Shared decisions & glossary deltas   (living — every task must honor these)
- **The at-rest read model is HETEROGENEOUS.** episode/chunk/community KV rows = `lunaris_core`
  primitives; the fact KV row = `lunaris_extract::Fact` (subject/object EntityIds; `structured_ingest`
  adds `source_episode_id` that `extract::Fact` deser drops); entity/relation are GRAPH nodes/edges,
  NOT KV. Browse/detail decode each kind at its real shape — never assume `core` primitives.
- **The shell is PUBLIC; data is not.** `GET /` carries no secret (recall token entered at runtime →
  localStorage); every `/v1/*` call it makes stays `scoped_auth("recall")`-gated. `claims.scope` is
  the only scope source.
- **Render API data via `textContent`/`createElement`, never a DOM HTML sink** — agent-supplied
  episode/fact text + entity names are untrusted (stored-XSS guard); CSP pins `connect-src 'self'`.

## Shared / risky contracts (freeze these first)
- `GET /v1/browse/{kind}` typed paginated list (cursor + next_cursor) -> read-api-pagination / browse-endpoints
- `GET /v1/detail/{kind}/{id}` primitive + provenance.source_episodes/confidence/entities -> detail-provenance
- `GET /v1/graph?root=&depth=` root-anchored neighborhood via graph_traverse -> graph-endpoint

## Tasks (breadth-first decomposition; detail lives in each TASK.md)
- [x] read-api-pagination  depends-on: none               — typed scan_page helper + cursor pagination over keyspace prefixes
- [x] browse-endpoints     depends-on: read-api-pagination — GET /v1/scopes + GET /v1/browse/{kind} JWT-scoped read surface
- [x] browse-shape-fix     depends-on: browse-endpoints    — browse/{kind} decodes the real at-rest shapes per kind
- [x] detail-provenance    depends-on: read-api-pagination — GET /v1/detail/{kind}/{id} primitive + resolved provenance
- [x] graph-endpoint       depends-on: none                — GET /v1/graph?root=&depth= root-anchored entity neighborhood
- [x] inspector-spa        depends-on: browse-endpoints,detail-provenance,graph-endpoint — the served single-file dashboard shell

## Exit criteria (observable; map each to the task that delivers it)
- [x] A reviewer loads the dashboard at `GET /` (public, self-contained, read-only)        (← inspector-spa · tests/inspector_ui.rs, 7/7)
- [x] picks a scope and pages every primitive kind without a query                          (← browse-endpoints + browse-shape-fix · tests/browse_endpoints.rs)
- [x] opens any primitive and sees its content + provenance (source episodes · confidence)  (← detail-provenance · tests/detail_provenance.rs)
- [x] renders an entity's graph neighborhood                                                (← graph-endpoint · tests/graph_endpoint.rs)
- [ ] **HUMAN-UAT**: open a real browser against a live Moon-backed server, paste a recall token, and confirm the four surfaces are understandable end-to-end (the goal's "understand … without writing a query" is a human judgment; 48/48 integration tests verify the served contract, not live DOM behaviour — no headless runner in this Rust repo).

# TASK: memory.dream_agenda — read-only distillation planner (Leiden-cluster ripe raw episodes + activation stats)

slug: dream-agenda · created: 2026-07-18 · stage: production
autonomy: auto
phase: done
<!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->

> engram-soul-loop **task 8a** (of the split dream wave — 8a=dream-agenda read-only planner,
> 8b=distill transactional apply). MILESTONE.md line 108-110: "Leiden-cluster raw episodes +
> activation stats → `memory.dream_agenda`". Tin's locked decision: **the coding harness IS the
> distiller/judge — Lunaris only maintains agendas + transactional apply-tools, no internal
> distillation LLM.** So this tool is READ-ONLY: it surfaces candidate clusters of ripe raw
> episodes with activation stats for the harness to reason over. It writes NOTHING.

---

## 0 · GROUND — the real codebase

Touches (files · symbols · signatures):
- `crates/lunaris-consolidate/src/ledger_reference_source.rs:35` — `LedgerReferenceSource::scan(&self, scope) -> Result<Vec<(Ulid, ActivationRecord)>, LunarisError>`. The clean, MVCC-safe full-scope activation read (KV rows, not versioned episodes). This is the candidate entry point.
- `crates/lunaris-consolidate/src/leiden.rs:90` — `leiden_pass(&GraphSnapshot, max_iters) -> CommunityAssignment`. Pure, deterministic label-propagation over `EntityId` nodes. **Currently UNWIRED (zero call sites).** `GraphSnapshot { nodes: Vec<EntityId>, edges: Vec<(EntityId, EntityId)> }`; `CommunityAssignment::community_members(node)`.
- `crates/lunaris-core/src/activation.rs:95,163` — `ActivationRecord`; `activation(now, decay) -> f64` (Anderson-1996 base-level). `DEFAULT_DECAY = 0.5`. NOTE 8b will add `archived_at`; 8a must treat a `None`/absent archived flag as live (no dependency on 8b, but skip records whose episode source is `distilled:*`).
- `crates/lunaris-core/src/keyspace.rs:200,224` — `episode_prefix(scope)`, `fact_prefix(scope)`; `episode_key(scope, id)`.
- `crates/lunaris-core/src/storage/port.rs:179,66` — `scan_range(scope, prefix, None)`, `graph_traverse`, `read_as_of(scope, key, at)`.
- `crates/lunaris-memory-service/src/verify_agenda.rs:118` — `hydrate_snippet`: `read_as_of(scope, &episode_key(scope,id), read_at)` → `serde_json::from_slice::<lunaris_core::Episode>` → `.content` trimmed 280, `""` on miss. **The exact hydration precedent** — reuse it (also read `.source`).
- `crates/lunaris-memory-service/src/record_decision.rs:83` — `handle(lunaris, scope, params)` service-handler shape; `EpisodeBuilder`, `scoped.ingest`. (8a does NOT write — but same handler signature/DTO discipline.)
- `crates/lunaris-memory-service/src/protocol.rs` — `MemoryRequest` enum + `scope()`/`name()`/dispatch arms (task-7 `VerifyAgenda`/`Resolve` are the newest precedent).
- `crates/lunaris-mcp/src/main.rs:461-503` — `#[tool(name="memory.verify_agenda")]` precedent; `crates/lunaris-mcp/tests/server_boot.rs:27` — `EXPECTED_TOOLS` (14 today → **15**).
- `crates/lunaris/src/handle.rs` — `ScopedLunaris`; `list_verify_agenda` (task 7) is the precedent for a thin read wrapper. Add `ScopedLunaris::dream_agenda(cfg) -> Result<DreamAgenda, LunarisError>`.
- `crates/lunaris/src/structured_ingest.rs:55` — `ingest_structured` writes a deduped graph via deterministic blake3 `EntityId` with `source_episode_id` on facts, **no LLM**. This is the TEST VEHICLE that lets the leiden path run in CI without an extractor.

Context (working folder): `.add/tasks/dream-agenda/`. Milestone `.add/milestones/engram-soul-loop/MILESTONE.md` (lines 108-113 task 8; line 44 "keep the distill-kind enum extensible for a future `procedure` kind" — informs 8b not 8a).

Honors (patterns / conventions):
- MCP outputSchema root MUST be `type:"object"` → response DTO is a FLAT struct with a `status` discriminator field, NEVER a `#[serde(tag)]` enum (CLAUDE.md §MCP). `server_boot.rs` is the only guard.
- Every request DTO carries `#[serde(deny_unknown_fields)]`; scope is server-bound, never wire-supplied (CLAUDE.md §HTTP DTO discipline).
- READ-ONLY: no `atomic_write`, no `ingest`, no ledger mutation. `grep -c atomic_write` on the new dream module + service handler = 0.
- Lock never across `.await`; `parking_lot` only. Keyspace helpers imported from `lunaris_core::keyspace`.
- **StubEmbedder rule (task-7 CI lesson):** any recall-based test MUST use `Lunaris::open_with_embedder("memory://", Arc::new(StubEmbedder::new(768)))`, never `Lunaris::open("memory://")` (NoopEmbedder → zero vectors → empty recall on CI).

Anchors the contract cites: `LedgerReferenceSource::scan`, `leiden_pass`, `ActivationRecord::activation`, `read_as_of`/`Episode`, `ScopedLunaris::dream_agenda`, `MemoryRequest::DreamAgenda`.

---

## 1 · SPECIFY — the rules

Feature: `memory.dream_agenda` — a read-only planner that groups ripe raw episodes into candidate distillation clusters, each annotated with activation stats + snippets, for the harness (the judge) to distill via `memory.distill` (8b).

Framings weighed:
- **Ledger-scan candidates (chosen)** — candidate set = episodes that have an activation record (referenced ≥1×). MVCC-safe, reuses `LedgerReferenceSource::scan`, honest signal.
- Full episode-prefix scan — would also catch never-referenced raw noise, but requires MVCC-correct version/tombstone handling across `episode_prefix`; deferred (v2 spec-delta). Flagged.
- Harness-side clustering only (flat candidate list, no grouping) — simpler, but milestone names Leiden; rejected in favor of the two-path grouping below.

Must:
<must>
  - Scan the activation ledger for `scope` (`LedgerReferenceSource::scan`); each `(ulid, record)` is a candidate.
  - Hydrate each candidate via `read_as_of` on `episode_key` → `Episode` (`.source`, `.content`). EXCLUDE candidates whose source starts with `distilled:` (never re-distill a distilled record) and whose episode is gone (read miss).
  - EXCLUDE candidates the record marks archived, if the archived marker is present (forward-compat with 8b's `archived_at`; absent marker = live).
  - Compute each candidate's activation = `record.activation(now, DEFAULT_DECAY)`. If `max_activation` is set, keep only candidates with activation ≤ ceiling ("ripe" = decayed/low-use).
  - Cluster candidates two ways, deterministically:
    - **Leiden path (when entity signal exists):** build episode→entity map by scanning facts (`fact_prefix`) and reading each fact's `source_episode_id` + entity ids; build a `GraphSnapshot` (nodes = candidate entities, edges = intra-episode entity co-occurrence); run `leiden_pass`; assign each episode to the community of its dominant entity. Episodes sharing an entity-community form one cluster.
    - **Source-class fallback (always available):** episodes with zero entities group by source-class bucket (the `source` prefix before `:`, e.g. `lunaris`, `edit`, `decision`).
  - Each cluster carries: a stable `cluster_id`, `size`, `member_episode_ids` (ULID strings, sorted), `mean_activation`, `max_activation`, `dominant_source` (most-common source-class among members), and up to 3 `snippets` (member `.content` trimmed ≤280 chars).
  - Drop clusters smaller than `min_cluster_size`; sort clusters by size DESC then mean_activation DESC; cap to `limit` clusters.
  - READ-ONLY: the call writes nothing (no episode, no ledger, no agenda row).
</must>
Reject:
<reject>
  - `limit == 0` or `limit > 100` → `"invalid_limit"` (clamp is NOT silent; explicit reject keeps the cap auditable). min_cluster_size > 100 → `"invalid_min_cluster_size"`.
  - `max_activation` present but NaN → `"invalid_max_activation"`.
  - (scope is server-bound; a wire-supplied `scope`/`tenant` field → serde `deny_unknown_fields` reject.)
</reject>
After:
<after>
  - The response lists 0..=limit clusters, each with ≥min_cluster_size members, sorted, activation stats populated, snippets ≤3.
  - Storage is byte-for-byte unchanged (read-only): no new keys under `episode_prefix`, `activation_prefix`, `fact_prefix`, or `verify_agenda_prefix`.
  - `distilled:*` sources never appear as members.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ **Leiden path is worth the facts-scan complexity in v1** — lowest confidence: reading+parsing every fact to build episode→entity is the heaviest part and where an executor stalls; if the facts read proves blocked/fragile, the **source-class fallback alone is an acceptable v1 descope** (flag it at the gate, spec-delta the leiden path). The source-class path is a MUST and guarantees green regardless. Cost if wrong: leiden clustering ships dead/untested — unacceptable, so the fallback must be the guaranteed-tested path and leiden must be exercised by the structured-ingest test or descoped, never merged untested (built≠wired).
  - [ ] Ledger-scan candidate set (referenced episodes only) is the right v1 universe — never-referenced raw noise is out of scope until the MVCC episode-scan lands (v2). Confirm the milestone's "raw episodes" intent is satisfied by referenced-raw for v1.
  - [ ] `DEFAULT_DECAY = 0.5` is the right decay for the ripeness computation (matches consolidator + boost read path).
</assumptions>

---

## 2 · SCENARIOS — pass/fail cases

<scenarios>

```gherkin
Scenario: source-class grouping over referenced raw episodes (always-available path)
  Given a scope with 3 referenced episodes source "lunaris:tool_call:post" and 2 source "edit:<scope>"
  And each has an activation ledger record (referenced at least once)
  When memory.dream_agenda is called with limit=20, min_cluster_size=1
  Then the response groups them into a "lunaris" cluster (size 3) and an "edit" cluster (size 2)
  And each cluster carries mean_activation, max_activation, dominant_source and ≤3 snippets
  And storage is unchanged (no new keys written)

Scenario: leiden entity-clustering when structured facts exist (no-LLM path)
  Given two episodes ingested via ingest_structured sharing entity "amber-relay"
  And a third episode sharing no entity with them
  When memory.dream_agenda is called
  Then the two entity-sharing episodes land in ONE cluster
  And the third is not a member of that cluster
  And leiden_pass was the mechanism (community-derived cluster_id, not a source bucket)

Scenario: distilled records are never candidates
  Given a referenced episode with source "distilled:lesson:<scope>"
  And a referenced raw episode source "lunaris:tool_call:post"
  When memory.dream_agenda is called
  Then only the raw episode appears as a cluster member
  And the distilled:* episode is excluded

Scenario: max_activation ceiling keeps only ripe (decayed) episodes
  Given one episode with high activation (many recent strong refs) and one decayed low-activation episode
  When memory.dream_agenda is called with max_activation just above the decayed value
  Then only the decayed episode is a candidate
  And storage is unchanged

Scenario: reject invalid limit
  Given any scope
  When memory.dream_agenda is called with limit=0 (or limit=101)
  Then it returns error "invalid_limit"
  And no scan or read is performed against storage beyond validation
```

</scenarios>

---

## 3 · CONTRACT — freeze the shape

```
TOOL memory.dream_agenda   (read-only; MCP + memory-service dispatch)

DreamAgendaParams  #[serde(deny_unknown_fields)]   body: {
    limit?: usize            // default 20; reject 0 or >100 -> "invalid_limit"
    min_cluster_size?: usize // default 1; reject >100 -> "invalid_min_cluster_size"
    max_activation?: f64     // optional ripeness ceiling; reject NaN -> "invalid_max_activation"
}

200 -> DreamAgendaResponse  (FLAT struct, root type:"object") {
    status: String           // "ok"
    total_candidates: usize   // candidates considered after exclusions/filter
    count: usize              // clusters.len()
    clusters: Vec<DreamClusterDto>
}
DreamClusterDto {
    cluster_id: String              // "com:<hex>" (leiden) | "src:<class>" (fallback)
    size: usize
    member_episode_ids: Vec<String> // ULID strings, sorted
    mean_activation: f64
    max_activation: f64
    dominant_source: String         // most-common source-class among members
    snippets: Vec<String>           // ≤3, each ≤280 chars
}
4xx -> { error: "invalid_limit" | "invalid_min_cluster_size" | "invalid_max_activation" }

Engine (lunaris-consolidate::dream):
    struct DreamConfig { limit, min_cluster_size, max_activation: Option<f64>, decay }
    struct DreamCluster { cluster_id, member_ids: Vec<Ulid>, mean_activation, max_activation, dominant_source, snippets }
    struct DreamAgenda { total_candidates: usize, clusters: Vec<DreamCluster> }
    async fn build_dream_agenda(storage: Arc<dyn StoragePort>, scope: &Scope, cfg: &DreamConfig, now: u64) -> Result<DreamAgenda, LunarisError>
Wrapper (lunaris::ScopedLunaris):
    async fn dream_agenda(&self, cfg: DreamConfig) -> Result<DreamAgenda, LunarisError>   // now = SystemTime::now()
Service (lunaris-memory-service::dream_agenda):
    async fn handle(lunaris: &Lunaris, scope: &Scope, params: DreamAgendaParams) -> Result<DreamAgendaResponse, ServiceError>
Dispatch: MemoryRequest::DreamAgenda { scope, params }; name "dream_agenda"; op "dream_agenda".
MCP: #[tool(name="memory.dream_agenda")]; EXPECTED_TOOLS 14 -> 15.

Access pattern: READ-ONLY. scan_range(activation_prefix) + scan_range(fact_prefix) + read_as_of(episode_key). Zero writes.
```

Status: FROZEN @ v1 — approved by Tin Dang (autonomous project-lead, engram-soul-loop standing directive)

**Lowest-confidence flag at freeze [contract]:** the Leiden path's facts-scan (⚠ §1). Mitigation baked into the contract: the source-class fallback is an independent MUST with its own scenario, so the suite is green even if entity data is absent; the leiden path is proven by the structured-ingest scenario (no LLM). If facts-scan is blocked at build, descope leiden to a gate-flagged spec-delta — never merge it untested.

---

## 4 · TESTS — failing-first suite (red)

Coverage target: 90% of the new dream module + service handler branches.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_source_class_grouping: ingest 3 "lunaris:tool_call:post" + 2 "edit:s" episodes (StubEmbedder), reference each (record_activation_refs / recall), dream_agenda(limit=20) → two clusters of size 3 and 2; assert stats populated + storage key-count unchanged.
  - test_leiden_entity_clustering: ingest_structured two episodes sharing entity + one disjoint → dream_agenda → the two share one cluster_id ("com:*"), the third is not a member.
  - test_distilled_excluded: one "distilled:lesson:s" + one raw referenced episode → only raw appears as member.
  - test_max_activation_ceiling: high-activation vs decayed episode → ceiling keeps only decayed.
  - test_reject_invalid_limit: limit=0 and limit=101 → "invalid_limit"; limit>100 min_cluster_size branch; NaN max_activation → "invalid_max_activation".
  - test_read_only_no_writes: snapshot storage key count before/after a dream_agenda call → identical.
  - server_boot roster: EXPECTED_TOOLS includes "memory.dream_agenda" (15 total) — real binary boot.
</test_plan>

Tests live in: `crates/lunaris-consolidate/src/dream.rs` (unit — leiden + grouping + validation), `crates/lunaris-memory-service/src/dream_agenda.rs` (service handler, StubEmbedder), `crates/lunaris-mcp/tests/server_boot.rs` (roster). MUST run red before Build.

---

## 5 · BUILD — AI writes code

Scope (may touch): `crates/lunaris-consolidate/src/dream.rs` `crates/lunaris-consolidate/src/lib.rs` `crates/lunaris/src/handle.rs` `crates/lunaris-memory-service/src/dream_agenda.rs` `crates/lunaris-memory-service/src/protocol.rs` `crates/lunaris-memory-service/src/lib.rs` `crates/lunaris-mcp/src/main.rs` `crates/lunaris-mcp/tests/server_boot.rs`
Strategy (ordered batches):
  1. RED: write all tests above (red for missing `build_dream_agenda` / handler / roster entry).
  2. GREEN engine: `lunaris-consolidate::dream` — candidate scan+hydrate+exclude, activation filter, source-class grouping, leiden path, sort/cap. Export from lib.rs.
  3. GREEN wrapper: `ScopedLunaris::dream_agenda`.
  4. GREEN service+MCP: handler, protocol dispatch, `#[tool]`, roster 15.
Safety rule: READ-ONLY — no atomic_write / ingest / ledger write in the dream module or handler (grep-assert 0). Lock never across await.
Constraints: do NOT change any test or the contract; keyspace helpers from lunaris_core; StubEmbedder for recall tests.

---

## 6 · VERIFY — evidence + non-functional review

- [ ] all tests pass (dream module + service + roster)
- [ ] coverage did not decrease
- [ ] no test or contract altered during build
- [ ] the green was EARNED — adversarial refute-read subagent; leiden path proven by structured-ingest test (not stubbed), source-class path proven independently
- [ ] READ-ONLY confirmed: `grep -c atomic_write` on dream.rs + dream_agenda.rs = 0; storage key-count unchanged across a call (test asserts it)
- [ ] no lock held across await
- [ ] layering: dream module in lunaris-consolidate; service → lunaris wrapper (no new cross-crate cycle)

### Build expectations — what "correct" looks like
- [ ] `distilled:*` sources never appear as cluster members — confirmed by test_distilled_excluded
- [ ] leiden_pass gains its FIRST real call site (was zero) — confirmed by grep + test_leiden_entity_clustering asserting a "com:*" cluster_id
- [ ] MCP roster shows 15 tools incl memory.dream_agenda — confirmed by server_boot.rs real-binary boot
- [ ] a dream_agenda call writes nothing — confirmed by before/after storage key-count assertion

### Deep checks
- [ ] WIRING — build_dream_agenda called by ScopedLunaris::dream_agenda called by service handler called by MCP tool; leiden_pass called by build_dream_agenda
- [ ] DEAD-CODE — no orphaned symbol
- [ ] SEMANTIC — n/a (code task)

### GATE RECORD
Outcome: PASS
Evidence (orchestrator re-verified, not trusting the executor):
- Tests 13/13 green — consolidate `dream::tests` 6/6 (source_class_grouping, leiden_entity_clustering, distilled_excluded, max_activation_ceiling, reject_matrix, writes_nothing); memory-service `dream_agenda::tests` 6/6 (incl. `dream_agenda_clusters_via_ingest_structured_and_ledger` — full stack, no-LLM leiden proof); mcp `server_boot` 1/1 (15 tools, real binary).
- READ-ONLY confirmed: `atomic_write` in dream.rs appears ONLY past `#[cfg(test)]` (line 371) — production `build_dream_agenda`/`handle` call none; `build_dream_agenda_writes_nothing` asserts storage key-count unchanged.
- Leiden FIRST real call site: dream.rs:227 (was zero); proven end-to-end by the ingest_structured integration test asserting a `com:`-prefixed cluster.
- `cargo clippy --workspace --all-targets -- -D warnings` clean (1m55s, all crates incl test targets compiled). `cargo fmt --all` applied (only vendor/moon submodule shows a pre-existing non-our diff; pin e41aa671 intact).
- Green EARNED: integration test is discriminating (real shared blake3 entity → one `com:` cluster through the full handle→wrapper→engine stack), StubEmbedder harness, not overfit/vacuous.
- No contract deviation. One internal correctness fix (MVCC read_at +999ms round-up) — not a contract change.
Reviewed by: Tin Dang (autonomous project-lead, adversarial orchestrator re-verify) · date: 2026-07-18

---

## 7 · OBSERVE — feed the next loop

Watch: dream_agenda p50 latency (full-scope ledger + facts scan — watch on large scopes); cluster count distribution.

### Spec delta
- [SPEC · open] full episode-prefix scan to include never-referenced raw noise as candidates (MVCC version/tombstone-correct) — v2 (evidence: v1 ledger-scan only surfaces referenced episodes).
- [SPEC · seeded → distill] 8b `memory.distill` consumes these clusters: writes typed record (kind ∈ decision|lesson|invariant|gotcha, extensible to `procedure`), archives sources via `archived_at`, extends source_priority + digest prefixes.

### Competency deltas
- [ADD · open] splitting a milestone "task" into 8a/8b for red/green reviewability + executor reliability (evidence: task-8 size vs executor-stall history).

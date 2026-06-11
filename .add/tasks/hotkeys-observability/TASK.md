# TASK: HOTKEYS hot-key observability surface

slug: hotkeys-observability · created: 2026-06-11 · stage: production
phase: done   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Surface Moon's `HOTKEYS` SpaceSaving sketch as a Lunaris observability signal — a new `lunaris_hotkey_samples{scope, kind}` Prometheus gauge on the existing lunaris-server `/metrics` endpoint, fed by a 10s background poller that classifies raw Moon keys into bounded (scope, kind) labels.

Ground facts (vendor/moon src/storage/hotkey.rs + src/command/server_admin.rs + live probe @6390, 2026-06-11):
  - `HOTKEYS [COUNT n]` (n ∈ 1..=128, default 10) returns `[key, sampled_count]` pairs, count-descending; 1-in-64 sampling (HOTKEY_SAMPLE_RATE=64 multiplier to approx command rate); SpaceSaving sketch K=128, cumulative since process start (no decay, overestimates never underestimates); shard-merged server-side; `MOON_NO_HOTKEYS=1` kill switch ⇒ empty reply is legal.
  - The typed moon SDK has NO hotkeys wrapper — raw RESP like the HSCAN/FT.NAVIGATE precedents.
  - Lunaris key shapes observable in HOTKEYS: KV `lunaris:{scope}:{kind}:{ulid}` (core keyspace.rs), FT doc `lunaris_{scope}_{kind}_idx:{hex}` (moon keyspace ft_index_name + `:` + hex id), graph `lunaris_{scope}_graph`; anything else (other tenants of the Moon instance, system keys) is NOT Lunaris's.
  - lunaris-server precedents: queue_depth StoragePort default-NotSupported method + queue_depth_poller.rs (10s interval, shutdown Notify, warn-once on NotSupported) + metrics.rs D-25 bounded-label discipline.

Framings weighed: /metrics gauge with (scope, kind) aggregation via background poller (chosen — reuses the queue_depth poller + D-25 label discipline; per-key labels would be unbounded cardinality, per-(scope,kind) is bounded by live tenant count × ~10 kinds) · MCP admin tool (deferred — touches the MCP tool roster + server_boot guard for an operator-not-agent concern) · raw passthrough HTTP endpoint exposing key names (rejected — leaks cross-tenant key material through an unauthenticated operator endpoint) · push counters at call sites inside Lunaris (rejected — Moon already measures server-side; duplicating client-side misses non-Lunaris load).
Scope boundary: Moon backend + lunaris-server only; SQLite/Postgres stay NotSupported (gauge silent-absent); no MCP changes; no new HTTP route (rides existing /metrics); raw key names NEVER appear in metric labels.
Must:
<must>
  - `HotKey { key: Vec<u8>, sampled_count: u64 }` type + additive `StoragePort::hot_keys(&self, count: usize) -> Result<Vec<HotKey>, StorageError>` default `Err(NotSupported("hot_keys_unsupported: …"))` (queue_depth precedent; no capability flag, poller handles NotSupported)
  - Moon override: raw RESP `HOTKEYS COUNT <n>` with n clamped to 1..=128; parses `[key, count]` array; empty reply (kill switch / cold server) ⇒ Ok(vec![])
  - `classify_hot_key(&[u8]) -> Option<(Scope, &'static str)>` in lunaris-server: KV keys → kind ∈ {episode, chunk, entity, relation, fact, community, doctree, kv-other}; FT doc keys → {ft-chunks, ft-entities, ft-facts, ft-communities}; graph keys → {graph}; non-Lunaris/unparseable → None (dropped, never labeled); scope segment re-validated via Scope::new before becoming a label
  - `lunaris_hotkey_samples{scope, kind}` IntGaugeVec registered in metrics.rs (10th metric, D-25 table extended); poller (10s, mirrors queue_depth_poller shutdown/warn-once shape) calls hot_keys(128), classifies, SUMS sampled_count per (scope,kind), `.reset()` before each apply so keys falling out of the top-128 don't leave stale series
  - moon-it integration test: hammer a known key (≥2000 reads, beats 1-in-64 sampling), assert hot_keys returns it; server-side classification unit table incl. adversarial keys (other-tenant, invalid scope chars, invalid UTF-8)
</must>
Reject:
<reject>
  - backend without support (SQLite/Postgres/mock) -> poller warns ONCE, gauge stays absent; /metrics still 200 (NotSupported is not an error path)
  - non-Lunaris or malformed key in HOTKEYS reply -> classified None, silently dropped (never a metric label, never a log per-key)
  - scope segment failing Scope::new -> None (label-injection defense; the parser is the last gate)
  - count out of range on the port method -> clamped to 1..=128 inside the Moon impl (never a Moon "COUNT must be between 1 and 128" error surfaced to the poller)
</reject>
After:
<after>
  - operators see per-scope/kind hot-key sample pressure on the existing /metrics scrape with zero new auth surface and zero raw key leakage
  - `lunaris_hotkey_samples` ranks which tenant + primitive kind dominates Moon traffic (multiply by 64 for approx command rate)
  - non-Moon deployments are untouched (gauge absent, one warn line at startup)
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ HOTKEYS sampled counts are CUMULATIVE since process start (no decay), so the gauge is monotone-ish per key but can swap membership as the sketch evicts — operators must read it as "pressure ranking", not a rate; if wrong (some decay exists): gauge semantics still hold, doc line adjusts. Cost if misread: alert rules on absolute values misfire — mitigated by HELP text naming the 1-in-64 sample multiplier + ranking semantics.
  ⚠ One Moon instance may host NON-Lunaris keys; classify must drop them rather than aggregate into an "other" bucket — if an "other" bucket is silently dominated by another tenant's keys the metric misleads. Chosen: drop (None) + a single `lunaris_hotkey_samples_dropped_total` counter? NO — keep scope minimal: drop silently; revisit if operators ask. Cost if wrong: invisible non-Lunaris pressure (operators still have raw HOTKEYS via redis-cli).
  - [x] HOTKEYS works on Moon v0.3.0 @6390 — live-probed this session (returned FT doc keys from the quantization eval traffic)
  - [x] `IntGaugeVec::reset()` exists on prometheus 0.14 (MetricVec::reset — verified in docs)
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: live Moon reports a hammered key through the port (moon-it)
  Given a live Moon and ≥2000 reads of one known Lunaris KV key
  When storage.hot_keys(128) runs
  Then the reply contains that key with sampled_count ≥ 1
  And an unsupported backend (mock/SQLite) returns NotSupported("hot_keys_unsupported…") instead

Scenario: classification maps every Lunaris key shape and drops the rest
  Given keys lunaris:acme.a1:fact:01H…, lunaris_acme.a1_chunks_idx:00ff…, lunaris_acme.a1_graph,
        plus other-tenant "sess:123", scope-invalid "lunaris:bad/scope:fact:01H…", and invalid UTF-8
  When classify_hot_key runs on each
  Then the three Lunaris keys yield (acme.a1, fact|ft-chunks|graph) respectively
  And the other three yield None — no label, no panic

Scenario: poller aggregates into the gauge and clears stale series
  Given a mock storage whose hot_keys returns fact+chunk keys for scopes A and B
  When the poller ticks twice (second tick returns only scope A)
  Then after tick 1 /metrics gather shows lunaris_hotkey_samples for both scopes summed per (scope,kind)
  And after tick 2 scope B's series is GONE (reset-before-apply), scope A's reflects the new counts

Scenario: NotSupported backend stays quiet and healthy
  Given a storage whose hot_keys returns NotSupported
  When the poller ticks repeatedly
  Then exactly one warn is logged and the gauge has zero series
  And /metrics keeps serving 200 with the other nine metrics intact

Scenario: count clamp at the Moon boundary
  Given hot_keys(0) and hot_keys(10_000) against live Moon
  When the override runs
  Then both succeed (clamped to 1 and 128) — Moon's "COUNT must be between 1 and 128" error never surfaces
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
NEW TYPE (lunaris-core storage/types.rs):
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  pub struct HotKey { pub key: Vec<u8>, pub sampled_count: u64 }

PORT (lunaris-core storage/port.rs — additive default method, queue_depth precedent):
  async fn hot_keys(&self, count: usize) -> Result<Vec<HotKey>, StorageError>
    default -> Err(NotSupported("hot_keys_unsupported: backend has no hot-key sketch"))
    NOTE: scope-less by design — HOTKEYS is an operator/server-global view, not tenant data.

MOON OVERRIDE (lunaris-storage-moon, raw RESP):
  HOTKEYS COUNT <clamp(count,1,128)>  -> Vec<HotKey> (empty reply => Ok(vec![]))

SERVER (lunaris-server):
  metrics.rs: 10th metric  lunaris_hotkey_samples  IntGaugeVec  labels [scope, kind]
    HELP: "Sampled hot-key pressure per scope+kind (1-in-64 sampling; multiply by 64
           for approx command count; SpaceSaving ranking, cumulative since Moon start)"
  hotkeys_poller.rs: spawn_hotkeys_poller(storage, shutdown) -> JoinHandle<()>
    every 10s: hot_keys(128) -> classify -> reset() gauge -> sum sampled_count per (scope,kind) -> set
    NotSupported -> warn once then quiet; other errors -> warn per cycle (queue_depth_poller shape)
  classify_hot_key(key: &[u8]) -> Option<(Scope, &'static str)>
    "lunaris:{scope}:{kind}:…"        -> kind ∈ {episode,chunk,entity,relation,fact,community,doctree} else "kv-other"
    "lunaris_{scope}_{kind}_idx[:..]" -> "ft-chunks"|"ft-entities"|"ft-facts"|"ft-communities"
    "lunaris_{scope}_graph"           -> "graph"
    anything else / invalid scope / invalid UTF-8 -> None
    kind label set is CLOSED (static strs) — D-25 cardinality: scope = live tenant set, kind ≤ 13.
  lib.rs: spawn next to spawn_queue_depth_poller, same shutdown Notify.
Schema: no storage schema change; /metrics text format gains one metric family.
```

Status: FROZEN @ v1 — approved by Tin Dang (2026-06-11, freeze #5)
Least-sure flag surfaced at freeze:
  ⚠ [spec] HOTKEYS counts are cumulative-since-start with sketch eviction — the gauge is a pressure RANKING, not a rate; if operators alert on absolute values they misfire (mitigation: HELP text names the semantics + 64× multiplier).
  ⚠ [contract] scope-less port method means ANY future multi-tenant server exposes all scopes' label names on /metrics — acceptable for the internal-first operator endpoint, revisit if /metrics ever becomes tenant-facing.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every §2 scenario has an executable test; red = compile failure on missing HotKey type / hot_keys method / classify_hot_key / hotkeys gauge.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - lunaris-core tests/hot_keys_port.rs: default port method returns NotSupported("hot_keys_unsupported…"); HotKey serde round-trip
  - lunaris-storage-moon tests/hotkeys_live.rs (moon-it): hammer one KV key with ≥2000 GETs via the typed client, hot_keys(128) contains it with sampled_count ≥ 1; hot_keys(0) and hot_keys(10_000) both Ok (clamp); empty-sketch fresh server tolerated (skip-safe assert shape)
  - lunaris-server tests/hotkeys_metrics.rs: classify_hot_key table test (3 Lunaris shapes → Some, other-tenant/bad-scope/non-UTF-8 → None); poller-tick aggregation against a mock StoragePort (two scopes summed per kind; second tick drops scope B series via reset-before-apply); NotSupported mock → zero series + /metrics text still contains the other metric families; lunaris_hotkey_samples appears in gather() after a tick
</test_plan>

Tests live in: `crates/lunaris-core/tests/hot_keys_port.rs` · `crates/lunaris-storage-moon/tests/hotkeys_live.rs` · `crates/lunaris-server/tests/hotkeys_metrics.rs` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): raw backend key names must NEVER become metric labels — every label pair passes through classify_hot_key's closed kind set + Scope::new re-validation.
Code lives in: `crates/lunaris-core/src/storage/{types.rs, port.rs}` · `crates/lunaris-storage-moon/src/hotkeys.rs` · `crates/lunaris-server/src/{metrics.rs, hotkeys_poller.rs, main.rs}`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

Build notes:
- Two red-suite touch-ups during build, neither weakening an assertion: prometheus API misuse fix (`get_gauge().value()` → `.get_value()` — compile error, the assertion is unchanged) and a clippy type-alias for the classify table (cosmetic).
- `poll_once` returns `bool` (supported?) so the spawn loop owns the warn-once flag — slightly different plumbing than queue_depth_poller's per-topic closure but the same observable discipline.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — hot_keys_port 2/2; hotkeys_live 2/2 live vs Moon v0.3.0 @6390 (0 SKIP lines confirmed; server-side `HOTKEYS COUNT 3` shows the hammered keys at sampled_count=64 — exactly 4096/64, the documented sampling rate); hotkeys_metrics 2/2; moon lib units 4/4 new (71 total); workspace sweep green except pre-existing tree_recall stale-6380 (re-verified 3/3 vs 6390 earlier this session)
- [x] coverage did not decrease — 8 new tests (2+2+2 integration + 4 moon parse units + closed-set pin), none removed
- [x] no test or contract was altered during build — two §5-noted non-weakening touch-ups (compile-fix + clippy alias); §3 untouched
- [x] concurrency safe — poller mirrors queue_depth_poller verbatim (interval+Notify select, biased); warn-once RwLock is read-then-write pure CPU, never held across .await; gauge reset+set are atomic prometheus ops (a /metrics scrape racing a tick sees either old or new series — same exposure class as every gauge)
- [x] no secrets / injection / unexpected deps — label-injection closed: scope re-validated via Scope::new, kind set closed (13 static strs), unparseable keys dropped; no new dependencies; raw keys never serialized to any HTTP surface
- [x] layering — HotKey + port default in lunaris-core (additive, object-safe); raw RESP in lunaris-storage-moon; classification + gauge in lunaris-server (Moon key-shape knowledge bleeds into the server's classifier — accepted: the FT/graph shapes are stable keyspace contracts re-exported from lunaris-storage-moon docs, recorded as a delta)
- [x] reviewed — auto-resolved under autonomy:auto; manual diff review of all 6 source files before commit

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — HotKey: types.rs → port.rs default → moon override (lib.rs `hot_keys` → hotkeys.rs) → poller `storage.hot_keys(128)`; gauge: metrics.rs registration → poll_once set → asserted via prometheus::gather() in hotkeys_metrics.rs; poller spawned in main.rs:88 next to the queue-depth poller (same Shutdown notify) — every new symbol referenced on the production path
- [x] DEAD-CODE (code) — clippy --workspace --all-targets clean; classify kinds all reachable (table test covers each); POLL_INTERVAL/HOTKEYS_COUNT consts used by spawn + tests
- [x] SEMANTIC (prose) — metrics.rs D-25 table extension + HELP text read in full: names the 1-in-64 multiplier, the SpaceSaving ranking semantics, and the closed cardinality bounds (⚠ flag #1 mitigation present verbatim)

### GATE RECORD
Outcome: PASS
Reviewed by: auto-resolved (autonomy: auto; complete evidence, no security finding; the one security-adjacent surface — label injection — is positively tested) · date: 2026-06-11

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): `lunaris_hotkey_samples` series count per scrape (cardinality regression = classifier leak); warn-once "hot_keys unsupported" line on non-Moon deploys (expected exactly once); operator confusion between sampled ranking and command rate (HELP text is the guard)
Spec delta for the next loop: Moon could expose a `HOTKEYS RESET`/windowed variant — cumulative-since-start counts make recent-pressure ranking degrade on long-lived servers (old hot keys never decay out); upstream feature request candidate alongside the FT.INFO quantization probe.

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
- [SDD · folded] Moon key-shape knowledge (FT doc / graph key grammar) now lives in THREE places: lunaris-storage-moon::keyspace (mint), lunaris-server::classify_hot_key (parse), docs — a shared reverse-parser in lunaris-core (next to parse_scope_from_key) would single-source it (evidence: classify_hot_key reimplements ft_index_name's shape backwards)
- [TDD · folded] sampling-based live tests need a deterministic traffic→expectation ratio — pipelining exactly 4096 GETs gave sampled_count=64 == 4096/64, turning a probabilistic assert into an exact one (evidence: hotkeys_live + redis-cli cross-check)
- [UDD · folded] cumulative-sketch gauges read as rates by default — HELP text must carry semantics ("ranking, not rate") because operators will alert on it without reading docs (evidence: ⚠ flag #1, mitigated in HELP)

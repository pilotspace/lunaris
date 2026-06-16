# TASK: Typed paginated list helper over scan_range (Page<T> + cursor)

slug: read-api-pagination · created: 2026-06-16 · stage: production
autonomy: auto   <!-- inherited from the project default (PROJECT.md); explicit level: manual < conservative < auto (visible · overridable) — lower below if a high-risk task needs it. -->
phase: done   <!-- ground -> specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower the
     autonomy level to `manual` or `conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 0 · GROUND — the real codebase ▸ docs/02-the-flow.md

Touches (files · symbols · signatures):
- `crates/lunaris-core/src/storage/port.rs:168` — `StoragePort::scan_range(&self, scope: &Scope, prefix: &[u8], as_of: Option<Hlc>) -> Result<BoxStream<'_, Result<(Bytes, Bytes), StorageError>>, StorageError>` — the ONLY enumeration primitive; streams `(key_bytes, value_bytes)` where value = serde_json of the primitive; per-row `Result` so mid-stream drops surface, not truncate. No start-key arg — only a prefix.
- `crates/lunaris-core/src/keyspace.rs:172` — `fact_prefix(&Scope) -> Vec<u8>` (+ `episode_prefix`/`chunk_prefix`/`entity_prefix`/`relation_prefix`/`community_prefix`/`doctree_prefix`) — each yields `lunaris:{scope}:{kind}:`; the per-kind scan prefix.
- `crates/lunaris-core/src/keyspace.rs:194` — `parse_scope_from_key(&[u8]) -> Option<Scope>` — safe byte→key parser; the model to follow if the helper extracts ULID/kind from a raw key.
- `crates/lunaris-core/src/primitives.rs` — `Episode`/`Chunk`/`Entity`/`Relation`/`Fact`/`Community` (all `Serialize + Deserialize`, `id: Ulid`, carry `bt: BiTemporal`) — the typed targets the value bytes deserialize into.
- `crates/lunaris-core/src/hlc.rs` — `Hlc` · `crates/lunaris-core/src/scope.rs` — `Scope` (the partition key).
- Consumer (sibling task `browse-endpoints`): `crates/lunaris-server/src/routes/` will call this helper through `StoragePort`, so it must be reusable by server + engine.

Context (working folder): candidate home = `crates/lunaris-core/src/storage/` (StoragePort + keyspace helpers already live in lunaris-core). No new config/data/migrations — pure read-model helper over an existing primitive.

Honors (patterns / conventions):
- Keyspace helpers belong in `lunaris-core`, imported from `lunaris_core::keyspace` — never a local key minter (RFC 0001 / RC-1).
- `Scope` is the partition key; the helper takes `&Scope` and never trusts a wire-side scope.
- `scan_range` item is a per-row `Result` — handle mid-stream errors, do NOT silently truncate the page.
- Lock discipline: helper is async over a stream — snapshot under guard, drop before `.await`. Keep the `.rs` file < 1000 lines (split read/write).

Anchors the contract cites (the symbols §3 will name): `StoragePort::scan_range`, the `*_prefix(&Scope)` helpers, the six primitive types, `Hlc`, `Scope`, `BiTemporal`. NEW surface §3 freezes: `Page<T> { items: Vec<T>, next_cursor: Option<String> }` + the `list_kind`/`scan_page` helper signature — placement, the `as_of` passthrough, and the **opaque cursor format** (the riskiest decision: `scan_range` exposes no start-key, so a page = scan-prefix → skip-past-cursor → `take(limit)`, with the cursor = last key/ULID; this only paginates correctly if `scan_range` yields keys in a stable ULID-sortable order — NOT guaranteed on Moon SCAN).

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: `scan_page<T>` — a scope-bound, typed, forward-paginated read over the KV store; turns `StoragePort::scan_range`'s unordered stream into a deterministic `Page<T>` with an opaque cursor. The shared read-API envelope every browse endpoint binds to.
Framings weighed: free generic async helper in `lunaris-core::storage` over `&dyn StoragePort` (chosen — keeps StoragePort object-safe; generics live at the free-fn boundary, mirroring the `scan_range<K: Key>` deviation) · generic default method ON StoragePort (rejected — generic trait methods break object safety, the exact reason scan_range took `&[u8]`) · engine method on `Lunaris` (rejected — couples the read-model to the engine; `lunaris-server` could not call it without the full engine).
Must:
<must>
  - Given `(port, scope, prefix, cursor, limit)`, return up to `limit` items of `T` deserialized from the scan VALUES, in ascending key (ULID) order — the helper IMPOSES the order; it never trusts `scan_range`'s backend order.
  - `next_cursor` is `Some(token)` iff more items remain after this page under the prefix; `None` on the last page.
  - Feeding a returned `next_cursor` into the next call resumes STRICTLY AFTER the last returned key: across the full prefix every item is returned exactly once — none skipped, none repeated (forward-only, stable).
  - An absent / empty cursor starts from the first key under the prefix.
  - The cursor is OPAQUE to callers (an encoded last-key token); callers never construct or parse it.
  - Each item is the FULL deserialized primitive `T: DeserializeOwned` (not truncated).
  - The helper scans ONLY the caller-supplied scope-derived `prefix`; it never returns a key outside that prefix/scope.
  - `as_of: Option<Hlc>` is accepted and passed THROUGH to `scan_range` (Phase-1 callers pass `None` = current state); the helper does not interpret time.
</must>
Reject:
<reject>
  - `limit == 0` -> "invalid_limit"
  - `limit > MAX_PAGE` (cap = 500) -> "limit_too_large"
  - malformed / unparseable cursor token -> "invalid_cursor"
  - a scanned value that fails `serde_json` deserialization into `T` -> "corrupt_row" (surfaced as `Err`, NOT a silently dropped row — transparency over availability)
  - a mid-stream `scan_range` error -> propagated as `Err` (a partial page is NEVER returned as if complete)
</reject>
After:
<after>
  - The page holds ≤ `limit` items, ULID-ascending; same `(prefix, cursor, limit)` over the same snapshot yields the same page (deterministic).
  - Following `next_cursor` to exhaustion reproduces the full set under the prefix, once each.
  - No write occurs (pure read); no key outside `prefix` is touched.
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ [x] ACCEPTED @ freeze v1 — O(N)-per-page buffering is acceptable at Phase-1 scope sizes. Verified `lunaris-storage-moon/src/kv.rs:124` already does `SCAN MATCH <prefix>*` (unordered) + buffers the whole prefix into a `Vec`, so sorting adds NO new asymptotic cost on Moon. Known residual: a very large single-kind scope pages in O(N) memory — revisit as a Phase-2 backend-native cursored scan if it bites.
  - [x] CONFIRMED — cursor = last-key ULID is sufficient (ULIDs unique + lexicographically time-sortable; no tie-breaker needed).
  - [x] CONFIRMED — helper home = `lunaris-core::storage` over `&dyn StoragePort`.
  - [x] CONFIRMED — `corrupt_row` stops the page (transparency over availability) — right for a review surface.
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
# ── Musts ──────────────────────────────────────────────────────────────
Scenario: Typed page in ULID order, capped at limit
  Given a scope holding 7 facts with ULIDs u1<u2<…<u7 (written in shuffled order)
  When scan_page::<Fact>(port, scope, fact_prefix, cursor=None, limit=3)
  Then it returns [u1,u2,u3] as fully-deserialized Fact values, ascending by ULID
  And next_cursor is Some

Scenario: Last page carries no cursor
  Given a scope holding 3 facts
  When scan_page::<Fact>(…, cursor=None, limit=10)
  Then it returns all 3 facts
  And next_cursor is None

Scenario: Following the cursor returns every item exactly once
  Given a scope holding 7 facts u1..u7
  When paging with limit=3, following next_cursor until it is None
  Then the concatenation of all pages is exactly [u1..u7] in ULID order, none skipped, none repeated

Scenario: A cursor resumes strictly after the last returned key
  Given a first page [u1,u2,u3] returned next_cursor c
  When scan_page::<Fact>(…, cursor=Some(c), limit=3)
  Then it returns [u4,u5,u6]
  And u1,u2,u3 do not reappear

Scenario: An empty cursor starts from the first key
  Given a scope holding facts u1..u5
  When scan_page::<Fact>(…, cursor=None, limit=2)
  Then the first returned item is u1

Scenario: Opaque cursor round-trips verbatim
  Given next_cursor token t from page 1 (caller does not parse it)
  When t is passed back unmodified as the cursor for page 2
  Then page 2 resolves correctly from the item after the page-1 tail

Scenario: Items are the full primitive, not truncated
  Given a fact with long fact_text, an embedding, and non-empty provenance
  When scan_page::<Fact>(…) returns it
  Then the returned Fact equals the stored Fact across all fields (fact_text, embedding, provenance, bt, confidence)

Scenario: Scope isolation — never a key outside the prefix
  Given scope A and scope B each hold facts
  When scan_page::<Fact>(port, scope=A, fact_prefix(A), …)
  Then every returned item belongs to scope A and no scope-B key appears

Scenario: as_of is passed through to scan_range untouched
  Given as_of = Some(hlc)
  When scan_page(…, as_of) runs against a recording fake port
  Then the port observed scan_range called with the same as_of (Phase-1 callers pass None → current state)

# ── Rejects (each asserts what stays unchanged) ────────────────────────
Scenario: Zero limit is rejected before any scan
  Given limit = 0
  When scan_page(…)
  Then it returns Err "invalid_limit"
  And the port's scan_range was never called

Scenario: Over-cap limit is rejected before any scan
  Given limit = 501 (MAX_PAGE = 500)
  When scan_page(…)
  Then it returns Err "limit_too_large"
  And the port's scan_range was never called

Scenario: A malformed cursor is rejected before any scan
  Given cursor = "not-a-real-token"
  When scan_page(…)
  Then it returns Err "invalid_cursor"
  And no items are returned and the port's scan_range was never called

Scenario: A value that won't deserialize stops the page
  Given a scanned value under the prefix that is not valid JSON for Fact
  When scan_page::<Fact>(…)
  Then it returns Err "corrupt_row"
  And no partial page is returned as if complete

Scenario: A mid-stream scan error propagates, never a short page
  Given scan_range yields two rows then an Err
  When scan_page::<Fact>(…, limit=10)
  Then it returns Err (the storage error)
  And the two rows seen before the error are not returned as a complete page
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```rust
// lunaris-core :: storage  — new module `list` (pure read-model helper; NO new crate deps)

pub const MAX_PAGE: usize = 500;

pub struct Page<T> {
    pub items: Vec<T>,                // ≤ limit, ascending by ULID key
    pub next_cursor: Option<String>,  // opaque; Some iff more remain, None on the last page
}

pub async fn scan_page<T: serde::de::DeserializeOwned>(
    port:   &dyn StoragePort,
    scope:  &Scope,
    prefix: &[u8],          // a lunaris_core::keyspace::*_prefix(scope) value
    cursor: Option<&str>,   // None / "" = from the first key
    limit:  usize,          // must be 1..=MAX_PAGE
    as_of:  Option<Hlc>,    // passed THROUGH to scan_range (Phase-1 callers pass None)
) -> Result<Page<T>, ListError>;

#[non_exhaustive]
pub enum ListError {           // impl: .code() -> &'static str  (the wire string)
    InvalidLimit,             // "invalid_limit"    — limit == 0
    LimitTooLarge,            // "limit_too_large"  — limit > MAX_PAGE
    InvalidCursor,            // "invalid_cursor"   — cursor not a valid ULID
    CorruptRow,               // "corrupt_row"      — a page value failed serde_json::from_slice::<T>
    Storage(StorageError),    // mid-stream / backend error, propagated
}
```

```
Cursor — the ULID parsed from the LAST returned key's suffix (the 26 chars after the final ':').
  Opaque to callers. An incoming cursor MUST parse via Ulid::from_string, else InvalidCursor.
  Because the prefix is constant per (scope, kind), lexicographic KEY order == ULID order, so the
  next page = keys strictly greater than (prefix ++ cursor_ulid). No base64, no bound on T.

Access pattern (READ-ONLY — no writes, no new tables/migrations):
  1. validate limit (1..=MAX_PAGE) and cursor (parse ULID) BEFORE any I/O   [→ the reject scenarios]
  2. scan_range(scope, prefix, as_of) -> drain the stream; the FIRST Err returns Storage(err)
  3. buffer (key, value) pairs; sort ascending by key
  4. window = pairs with key strictly > (prefix ++ cursor_ulid), then take(limit)
  5. deserialize ONLY the window's values into T; any failure -> CorruptRow
  6. next_cursor = Some(<ULID of the window's last key>) iff ≥1 pair remains after the window, else None
```

Least-sure flag surfaced at freeze: [spec] O(N)-per-page buffering accepted for Phase-1 — the helper buffers + sorts the whole prefix to impose order; verified Moon already buffers (`lunaris-storage-moon/src/kv.rs:124` `SCAN MATCH`), so no NEW asymptotic cost on Moon. Cost if wrong: a very large single-kind scope pages in O(N) memory → push pagination into a backend-native cursored scan (a Phase-2 storage change), not the helper.

Status: FROZEN @ v1 — approved by Tin Dang (2026-06-16). Changing this contract = a change request back to SPECIFY.
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: 95% (small, high-value core helper). Harness: a local `FakePort` impl `StoragePort` whose `scan_range` returns scripted `(key,value)` pairs (writable in shuffled order), records the `as_of` it was called with, and exposes a `scan_called` flag; all other trait methods `unimplemented!()`. `&dyn StoragePort` is dyn-safe (the scan_range `&[u8]` deviation keeps it object-safe), so the fake works behind `dyn`.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - test_typed_page_ulid_order: arrange 7 facts keyed u1..u7 fed shuffled / act scan_page::<Fact>(limit=3,cursor=None) / assert items==[u1,u2,u3] as Fact + next_cursor.is_some()
  - test_last_page_no_cursor: arrange 3 facts / act limit=10 / assert 3 items + next_cursor.is_none()
  - test_full_walk_each_once: arrange 7 facts / act page with limit=3 following next_cursor to None / assert concat==[u1..u7], no dup, no gap
  - test_cursor_resumes_after_tail: arrange page1==[u1,u2,u3] cursor c / act scan_page(cursor=Some(c),limit=3) / assert items==[u4,u5,u6] + u1..u3 absent
  - test_empty_cursor_from_first: arrange facts u1..u5 / act cursor=None,limit=2 / assert items[0]==u1
  - test_opaque_cursor_roundtrip: arrange next_cursor t from page1 passed back verbatim / act page2 / assert resolves from item after page1 tail
  - test_full_primitive_not_truncated: arrange fact w/ long text+embedding+provenance / act scan_page::<Fact> / assert returned Fact == stored Fact (all fields)
  - test_scope_isolation: arrange scopes A and B both with facts / act scan_page(scope=A, fact_prefix(A)) / assert every item scope==A + no B key
  - test_as_of_passthrough: arrange as_of=Some(hlc) + recording fake / act scan_page(as_of) / assert fake observed scan_range with same as_of
  - test_zero_limit_rejected: act limit=0 / assert Err code=="invalid_limit" + assert fake.scan_called==false
  - test_over_cap_rejected: act limit=501 / assert Err code=="limit_too_large" + assert fake.scan_called==false
  - test_malformed_cursor_rejected: act cursor=Some("not-a-real-token") / assert Err code=="invalid_cursor" + assert fake.scan_called==false
  - test_corrupt_row_stops_page: arrange a value under prefix that is not valid JSON for Fact / act scan_page::<Fact> / assert Err code=="corrupt_row" (no partial page)
  - test_midstream_error_propagates: arrange fake yields Ok,Ok,Err / act scan_page(limit=10) / assert Err is Storage(_) + the 2 prior rows are not returned as a complete page
</test_plan>

Tests live in: `crates/lunaris-core/tests/scan_page.rs` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Scope (may touch): `crates/lunaris-core/src/storage/list.rs` (new) · `crates/lunaris-core/src/storage.rs` (module decl + re-export — the 2018-style module file, NOT storage/mod.rs) · `crates/lunaris-core/src/lib.rs` (public re-export of `Page`, `scan_page`, `ListError`, `MAX_PAGE`)
Strategy (ordered batches): 1. types — `Page<T>`, `ListError` + `.code()` 2. input validation (limit `1..=MAX_PAGE`, cursor→`Ulid`) BEFORE any I/O 3. drain `scan_range`, buffer `(key,value)`, propagate first `Err` 4. sort by key, window = `key > prefix++cursor_ulid` then `take(limit)` 5. deserialize ONLY the window → `T`, set `next_cursor` from the window's last key suffix
Safety rule (feature-specific): validate every input before the `scan_range` call; on a mid-stream scan `Err`, return it — NEVER return the rows already seen as a complete page; the helper holds no lock across `.await` (pure stream consume).
Code lives in: `crates/lunaris-core/src/storage/`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

<!-- Scope tokens, backticked, FIRST declaring line: `./…` = this task dir · a token
     with "/" = project root · a bare name = sibling of the previous token's dir ·
     outside-root resolutions are dropped fail-closed · a DIRECTORY token covers its
     whole subtree (containment — diverges from §4's non-recursive counting) ·
     absent line = UNDECLARED (pre-existing tasks grandfathered, never retro-red) ·
     engine enforcement (touched ⊆ declared) lands in scope-gate-enforce.
     EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [x] all tests pass — `cargo test -p lunaris-core --test scan_page` 14/14; full crate 128/128
- [x] coverage did not decrease — net-new module + 14 new tests (one per scenario); no lines removed
- [x] no test or contract was altered during build — §3 FROZEN untouched; `tests/scan_page.rs` unchanged since red. Red→green by ADDING `src/storage/list.rs` + 2 re-export lines (`storage.rs`, `lib.rs`) only
- [x] the green was EARNED, not gamed — refute-read: shuffled fixtures force the sort (a no-sort impl fails `test_typed_page_ulid_order`); `was_scanned()==false` asserts validate-before-IO (a naive impl fails the 3 reject tests); `corrupt_row`/`midstream` use real bad JSON + an injected `Err` (a skip/partial impl fails them); the cursor-walk asserts no skip/repeat across boundaries. Discriminating, not vacuous
- [x] concurrency / timing safe — pure read; the helper holds NO lock and only `.await`s on the scan stream → no lock-across-await (CLAUDE.md invariant)
- [x] no exposed secrets, injection openings, or unexpected dependencies — ZERO new crate deps (reuses ulid/bytes/futures/serde_json/thiserror already in lunaris-core); no I/O beyond the injected `StoragePort`
- [x] layering & dependencies follow CONVENTIONS.md — helper sits in `lunaris-core` (bottom layer) over `&dyn StoragePort`; no upward dep; keyspace prefixes owned by `lunaris_core::keyspace`
- [x] auto-resolved (autonomy: auto) — the human froze §3 (the one approval); build→verify auto-gates on the evidence above. No security / concurrency / architecture residue to escalate

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [x] WIRING (code) — `scan_page`/`Page`/`ListError`/`MAX_PAGE` are referenced by `crates/lunaris-core/tests/scan_page.rs` (14 tests) AND re-exported at the crate root. Production consumers are the sibling tasks `browse-endpoints` + `detail-provenance` (both declared `depends-on read-api-pagination`); this task's deliverable IS the reusable read-API primitive, built first — NOT dead code. `cargo check --workspace` (minus py/ts cdylibs) compiles every dependent clean with the new re-exports
- [x] DEAD-CODE (code) — no orphaned symbol; private `ulid_from_key` is used by `scan_page`; `#![deny(unreachable_pub)]` + clippy `--all-targets -D warnings` both clean

### GATE RECORD
Outcome: PASS
Reviewed by: auto-gate (autonomy: auto) — §3 frozen by Tin Dang · date: 2026-06-16

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->

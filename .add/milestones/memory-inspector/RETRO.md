════════════════════════════════════════════════════════════════════════
 memory-inspector · Memory Inspector — Phase 1
════════════════════════════════════════════════════════════════════════
 VERDICT   DONE
 TASKS     6/6 done           CRITERIA  5/5 met
 GATES     6 PASS             WAIVERS   none

 goal  A reviewer can open a local browser, pick a scope, and browse and
       understand every captured memory — full content, provenance, and
       the entity graph — without writing a query.

 TASK                        PHASE     GATE TESTS PROGRESS
 ───────────────────────────────────────────────────────────────────────
 read-api-pagination         done      PASS 0     ●●●●●●●●●
 browse-endpoints            done      PASS 0     ●●●●●●●●●
 detail-provenance           done      PASS 0     ●●●●●●●●●
 graph-endpoint              done      PASS 0     ●●●●●●●●●
 inspector-spa               done      PASS 0     ●●●●●●●●●
 browse-shape-fix            done      PASS 0     ●●●●●●●●●
 legend  ● reached  ◉ current  ○ pending   spec→…→done

 EXIT CRITERIA  ●●●●●●●●●● 5/5 met

 LEARNINGS (7 carried)
   • SDD · open · A frozen contract note asserted "`serde_urlencoded`
     won't enforce `deny_unknown_fields`" — FALSE: axum `Query<T>`
     rejects unknown params with 400. The fix was to omit the attribute
     from read-only query DTOs (no overridable scope field → no
     smuggling vector) so the confirmed "ignore `?scope=`" behavior
     holds. Lesson: verify library-behavior claims before baking them
     into a contract; for `Query` DTOs, scope-safety comes from the
     handler reading `claims.scope` only, NOT from
     `deny_unknown_fields`. (evidence: `test_browse_ignores_wire_scope`
     went 400→200 after removing the attribute.)
   • ADD · open · The contract froze a security-relevant default
     (`/v1/scopes` cross-scope) behind a clearly-surfaced least-sure
     flag + explicit human confirm — the freeze flag did its job: the
     auto-gate accepted it as a signed design decision, not a silent
     risk. (evidence: §3 freeze note + §6 gate flag (b).)
   • TDD · open · A mock that records the `CypherQuery` STRING cannot
     catch a backend that mis-executes that string — the inline-filter
     bug passed graph-endpoint's gate and only surfaced in live UAT.
     Graph/Cypher contracts need a live-Moon discriminating test on the
     production path, not just a string-shape assertion (evidence:
     `graph_anchor_constrains.rs` is RED-worthy where the mock suite was
     green). Reinforces [[feedback_built_not_wired]].
   • SDD · open · "root-anchored neighborhood" was under-specified as a
     string template, not a behaviour — the freeze pinned the Cypher
     TEXT but not the observable "depth=1 excludes 2-hop nodes"
     property. Specify graph contracts by observable reachability, not
     query syntax (evidence: v1 froze the exact broken cypher and the
     gate passed). <!-- e.g. - [DDD · open] the model missed
     multi-tenancy (evidence: scenario_x failed) -->
   • TDD · open · browse-endpoints shipped green but prod-broken because
     its tests hand-seeded `core::Fact` rows instead of driving the real
     ingest path — the exact built ≠ wired trap. Every read-surface test
     MUST seed via a production write path (here
     `ingest_structured_inner`). (evidence:
     `test_browse_fact_via_real_ingest_structured` went 500→200 across
     this fix; the old suite never caught the 500.)
   • DDD · open · the domain has THREE at-rest shapes for "memory" (core
     primitives / extract::Fact / graph nodes+edges); the `core`
     primitives are an aspirational model, not the on-disk truth. The
     keyspace exposes `*_prefix` helpers for all six kinds even though
     entity/relation are never KV-populated by the happy path — a
     helper's existence ≠ data behind it. (evidence: traced
     ingest.rs/structured_ingest.rs/raptor.rs/verify worker 2026-06-16.)
   • SDD · open · a task premise can be contradicted by the codebase
     ("resolve provenance: Vec<Ulid>" — a field never serialized to
     disk); grounding surfaced it BEFORE a contract froze on it, and the
     human re-scoped. The ground phase earned its keep. (evidence:
     detail-provenance re-scoped + this fix-forward task spawned.)

 DECIDE NEXT  consolidate learnings + archive-milestone
              memory-inspector
════════════════════════════════════════════════════════════════════════
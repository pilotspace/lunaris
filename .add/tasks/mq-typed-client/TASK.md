# TASK: Typed moondb MqClient adoption in lunaris-storage-moon queue path

slug: mq-typed-client · created: 2026-06-11 · stage: production
phase: tests   <!-- specify -> scenarios -> contract -> tests -> build -> verify -> observe -> done -->
<!-- high-risk/method-defining scope? declare `risk: high` on the slug line above and lower
     the autonomy level with `autonomy: conservative` — the engine refuses an unguarded completion
     (`unguarded_high_risk_auto`, run.md guard). A comment is never a declaration. -->

> One file = one task. Fill sections top-to-bottom; the `add` skill drives each phase.
> When a phase is unclear, read its book chapter in `.add/docs/` (linked per section).
> The phase marker above is the single source of truth — keep it in sync via `add.py phase`.

---

## 1 · SPECIFY — the rules ▸ docs/03-step-1-specify.md

Feature: Typed moondb MqClient adoption in the Moon queue path (`crates/lunaris-storage-moon/src/queue.rs`)
Framings weighed: full-adopt SDK wire shape, `body` field (chosen) · hybrid (typed create/ack/dlq_len, raw push/pop keeping `partition`+`payload` fields) · upstream-first (add `push_fields` to moondb, then adopt)
Rationale: the `partition` wire field is write-only today — `parse_mq_pop` never reads it and `QueueMsg.partition` comes from the subscriber's own argument (queue.rs:120,157), so dropping it from the wire is behavior-preserving; full-adopt deletes the most raw RESP and the local parse code. Dotted `MQ.PUSH`/`MQ.POP` SDK helpers are server-UNHANDLED and must not be used (verified 2026-06-09, P-C spike).
Must:
<must>
  - `publish` enqueues via typed `MqClient::create` (idempotent, `max_delivery=None`) + `MqClient::push` — wire field `body`, payload bytes verbatim; returns the same monotonic offset derived from the stream entry id as today
  - `subscribe` polls via typed `MqClient::pop(topic, 1)` and acks via typed `MqClient::ack`; yields `QueueMsg{topic, partition, offset, payload}` with payload = the `body` field bytes
  - `queue_length` reads via typed `MqClient::dlq_len` (dead-letter depth stays the documented best-available signal)
  - `supports_native_queue` probe keeps its current semantics (DLQLEN on a probe topic; `unknown command` ⇒ false)
  - Zero `redis::cmd("MQ")` call sites remain in queue.rs (the module's only raw-RESP exemption stays kv.rs SCAN per STORE-09)
  - `publish_txn` EVALUATION: one env-gated integration test proving whether `MQ PUBLISH` inside `TXN.BEGIN…COMMIT` enqueues exactly-once-visible-after-commit on live Moon v0.3.0; verdict recorded in §7 OBSERVE
  - Scoped topic naming `lunaris:{scope}:{topic}` via `mq_topic` unchanged
</must>
Reject:
<reject>
  - MQ on a server without the command family -> publish/subscribe surface `StorageError::Backend` carrying "mq_unsupported"; `supports_native_queue` returns Ok(false), never errors
  - `MQ POP` reply that is neither Nil nor Array -> "mq_pop_unexpected_reply" (stream stays alive, error yielded to consumer — current behavior preserved)
  - A popped message missing the `body` field -> yielded with EMPTY payload (lenient, matches current `payload`-missing behavior; never panics, never drops the stream)
</reject>
After:
<after>
  - queue.rs is typed-SDK only; `parse_mq_pop`/`field_value`/`value_bytes` local parsers deleted (SDK `MqMessage` parse replaces them)
  - StoragePort surface (signatures, QueueMsg shape, offsets, at-least-once + ack-before-yield semantics) is byte-for-byte unchanged for callers
  - A recorded publish_txn verdict (works / server-rejects) exists for the follow-up decision on in-TXN queue events
</after>
Assumptions — lowest-confidence first:
<assumptions>
  ⚠ `MQ PUBLISH` (the subcommand publish_txn issues) is actually handled by Moon v0.3.0's dispatcher — lowest confidence because the 2026-06-09 spike found the dotted `MQ.POP` unhandled and we have never exercised `MQ PUBLISH`; if wrong: the evaluation Must records "server-rejects" and the in-TXN follow-up dies, but the swap itself is unaffected
  ⚠ No deployed consumer reads the `partition`/`payload` wire fields besides queue.rs itself — lowest confidence because Moon queues are reachable by any Redis client (Helios-side tooling could in principle POP directly); if wrong: their parser sees `body` instead of `payload` and reads empty payloads — needs a drain-before-deploy note in the migration doc
  - [ ] Typed `MqClient::pop` parses Moon's `MQ POP` array reply identically to our local parser for well-formed messages (SDK parse_mq_messages reads [id, [field,val,...]] — same shape)
  - [ ] In-flight old-format messages at deploy time are tolerable: consolidate queues drain in seconds and the empty-payload lenient path means no panic (migration note, not code)
</assumptions>

<!-- EXIT: every rule stated, every rejection named; assumptions ranked lowest-confidence first, the top one or two ⚠-flagged with why + cost (or, for trivial scope, an honest "none material" that still names the single biggest risk). -->

---

## 2 · SCENARIOS — pass/fail cases ▸ docs/04-step-2-scenarios.md

<scenarios>

```gherkin
Scenario: publish/subscribe round-trip preserves payload bytes on the new wire shape
  Given a live Moon v0.3.0 and scope "acme.agent-1"
  When publish(scope, "consolidate", partition 0, payload b"\x00binary\xff") then subscribe polls
  Then the yielded QueueMsg.payload equals b"\x00binary\xff" and offset > 0
  And the raw stream entry carries field "body" (not "payload") when inspected via MQ POP

Scenario: ack-before-yield prevents redelivery
  Given a published message consumed once by subscribe
  When the stream is polled again past the poll interval
  Then the same entry id is not redelivered (DLQ stays empty, pending list clear)
  And StoragePort at-least-once semantics are unchanged for crash-before-ack

Scenario: queue_length reports dead-letter depth via typed dlq_len
  Given a fresh scoped topic with no messages
  When queue_length(scope, "consolidate", 0) is called
  Then it returns 0 without error
  And the scoped topic name is lunaris:acme.agent-1:consolidate

Scenario: server without MQ family
  Given a Redis-compatible server lacking MQ (probe topic DLQLEN -> "unknown command")
  When supports_native_queue is called
  Then it returns Ok(false)
  And publish against it surfaces StorageError::Backend("mq_unsupported…") — no panic, no retry loop

Scenario: malformed POP reply keeps the stream alive
  Given subscribe is polling and the server returns a non-Nil, non-Array value
  When the tick processes the reply
  Then the consumer receives Err(StorageError::Backend("mq_pop_unexpected_reply…"))
  And the next poll tick still runs (stream not terminated)

Scenario: message missing the body field yields empty payload
  Given a legacy-format entry (fields partition+payload) sitting in the stream at deploy time
  When subscribe pops it
  Then it yields QueueMsg with empty payload and acks it (drains, never wedges)
  And no panic or stream termination occurs

Scenario: publish_txn evaluation verdict (evidence test, env-gated)
  Given a live Moon v0.3.0 with TXN.BEGIN open
  When MqClient::publish_txn(topic, body) then TXN.COMMIT
  Then the message is poppable after commit AND not poppable before commit — or the server rejects MQ PUBLISH
  And whichever outcome occurs is recorded verbatim in §7 OBSERVE (both outcomes pass the test)
```

</scenarios>

<!-- EXIT: one scenario per Must AND per Reject; each result is observable. -->

---

## 3 · CONTRACT — freeze the shape ▸ docs/05-step-3-contract.md

```
INTERNAL crate API (crates/lunaris-storage-moon/src/queue.rs) — signatures UNCHANGED, callers unaffected:
  publish(c: &MoonClient, scope: &Scope, topic: &str, partition: u16, payload: Bytes) -> Result<u64, StorageError>
  subscribe(client: MoonClient, scope: &Scope, _group: &str, topic: &str, partition: u16)
      -> Result<BoxStream<'static, Result<QueueMsg, StorageError>>, StorageError>
  queue_length(c, scope, topic, _partition) -> Result<u64, StorageError>
  supports_native_queue(c) -> Result<bool, StorageError>

WIRE contract (the part that CHANGES — frozen here):
  MQ CREATE lunaris:{scope}:{topic}                  via MqClient::create(key, None)   — idempotent
  MQ PUSH   lunaris:{scope}:{topic} body <payload>   via MqClient::push                — field name: "body" (was: partition <n> payload <bytes>)
  MQ POP    lunaris:{scope}:{topic} COUNT 1          via MqClient::pop(key, 1) -> Vec<MqMessage{id, data, ..}>
  MQ ACK    lunaris:{scope}:{topic} <entry_id>       via MqClient::ack
  MQ DLQLEN lunaris:{scope}:{topic}                  via MqClient::dlq_len -> i64
  FORBIDDEN: dotted MQ.PUSH / MQ.POP (server-unhandled) and any redis::cmd("MQ") in this module.
  partition: accepted by the API, NOT encoded on the wire (doc-comment states this; QueueMsg.partition continues to echo the subscriber's argument).
  offset derivation: stream_id_to_offset(entry_id) unchanged (ms*1_000_000+seq, saturating).

Error responses (every §1 Reject has one):
  mq_unsupported           -> StorageError::Backend, message prefix "mq_unsupported: " + server error  (publish/queue_length on MQ-less server)
  mq_pop_unexpected_reply  -> StorageError::Backend, message prefix "mq_pop_unexpected_reply: " + debug of value; stream continues
  missing body field       -> NOT an error: QueueMsg with payload = Bytes::new(), entry still ACKed

publish_txn evaluation artifact (evidence-only, NOT wired into atomic_write — that is a follow-up task gate):
  test `mq_publish_txn_probe` in crates/lunaris-storage-moon/tests/ env-gated on LUNARIS_TEST_MOON;
  passes on EITHER outcome; asserts visibility-after-commit XOR clean server rejection; verdict string lands in §7.

Schema: no persistent schema change; Moon stream entries change field layout (body) — transient data only, drain-before-deploy note in docs/migration.
```

Status: FROZEN @ v1 — approved by Tin Dang (baseline lock, 2026-06-11; flag 1 wire-format switch accepted)
<!-- The freeze IS the one approval — lead it with the bundle's lowest-confidence flag: the 1–2
     points most likely wrong across the whole bundle, tagged [spec|scenario|contract|test], each
     with why + cost (the §1 ⚠ assumptions feed it; a flag may point at a scenario or the contract
     too — see run.md). Approved -> Status: FROZEN @ vN — approved by <name>. Changing a frozen
     contract = change request back to SPECIFY.
     EXIT: frozen + every spec rejection has a contracted response + names match GLOSSARY + the
     bundle's lowest-confidence flag was surfaced at the freeze (or an honest "none material"). -->

---

## 4 · TESTS — failing-first suite (red) ▸ docs/06-step-4-tests.md

Coverage target: every §2 scenario has an executable test except "malformed POP reply" (fault-injection on a live server is impossible; covered by the SDK's lenient parse — non-array ⇒ empty vec ⇒ idle tick — and pinned in review). Red discriminators: wire-format test + legacy-entry test + static no-raw-MQ scan.
Plan (one test per scenario, asserting behavior not internals):
<test_plan>
  - mq_wire_format_is_body (moon-it): publish via StoragePort / raw-inspect the stream entry via redis MQ POP / assert field "body" carries the bytes (RED: today writes partition+payload)
  - publish_subscribe_roundtrip_preserves_bytes (moon-it): binary payload round-trip, offset > 0 (regression guard)
  - ack_prevents_redelivery (moon-it): consume once / re-poll window / assert no duplicate entry id, dlq 0
  - queue_length_zero_on_fresh_topic (moon-it): fresh scoped topic / queue_length / 0 + scoped name format
  - legacy_entry_missing_body_yields_empty_payload (moon-it): raw-push partition+payload entry / subscribe / assert EMPTY payload yielded + acked (RED: today reads `payload` field and yields the bytes)
  - mq_publish_txn_probe (moon-it): TXN.BEGIN / MqClient::publish_txn / assert not-poppable-before XOR clean rejection, poppable-after on commit; prints verdict for §7 (passes on either outcome BY DESIGN — evidence test)
  - queue_rs_uses_only_typed_mq_client (static, no feature gate): include_str! source scan asserts zero `redis::cmd("MQ")` in src/queue.rs (RED: 8 call sites today)
  - supports_native_queue gating: covered by existing lunaris-mcp WIRED tests + probe path unchanged; "server without MQ" scenario asserted against SQLite-backend conformance (existing suite, unchanged)
</test_plan>

Tests live in: `crates/lunaris-storage-moon/tests/mq_typed_client.rs` · `mq_typed_client_static.rs` · MUST run red (missing implementation) before Build.
<!-- declare paths as backticked tokens on this line: `./…` = this task dir ·
     a token with "/" = project root · a bare name = sibling of the previous
     token's dir · a directory counts its *.py files (non-recursive); reports
     mark declared counts with † · anything resolving outside the project root counts 0 -->

<!-- EXIT: one test per scenario; suite red for the RIGHT reason; target recorded. -->

---

## 5 · BUILD — AI writes code ▸ docs/07-step-5-build.md

Safety rule (feature-specific): <e.g. debit+credit in one atomic transaction>
Code lives in: `./src/`
Constraints: do NOT change any test or the contract; allow-list packages only; ask if unclear.

<!-- EXIT: all green; coverage held; no test/contract touched; no unlisted dependency. -->

---

## 6 · VERIFY — evidence + non-functional review ▸ docs/08-step-6-verify.md

- [ ] all tests pass
- [ ] coverage did not decrease
- [ ] no test or contract was altered during build
- [ ] concurrency / timing of the risky operation is safe
- [ ] no exposed secrets, injection openings, or unexpected dependencies
- [ ] layering & dependencies follow CONVENTIONS.md
- [ ] a person reviewed and approved the change

### Deep checks — do not skim (fill the path that applies; the resolver judges which)
- [ ] WIRING (code) — every new symbol is referenced; record where / how confirmed
- [ ] DEAD-CODE (code) — no new unused or orphaned symbol introduced
- [ ] SEMANTIC (prose / non-code) — read in full, not skimmed: <what read · what confirmed>

### GATE RECORD
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
If RISK-ACCEPTED -> owner: <name> · ticket: <link> · expires: <date>   (never for a security gap)
Reviewed by: <name> · date: <date>

<!-- A security finding is ALWAYS HARD-STOP. Record exactly one outcome — no silent pass. -->

---

## 7 · OBSERVE — feed the next loop ▸ docs/09-the-loop.md

Watch (reuse scenarios as monitors): <error rate / per-rejection rate / latency>
Spec delta for the next loop: <what production taught you>

### Competency deltas
What did this loop teach the foundation? One line each, tagged by competency
(`DDD · SDD · UDD · TDD · ADD`), status `open`, with evidence. See the `add` skill's `deltas.md`.
<!-- e.g.  - [DDD · open] the model missed multi-tenancy (evidence: scenario_x failed) -->

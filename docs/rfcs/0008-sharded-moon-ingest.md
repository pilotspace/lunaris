# RFC 0008 — Sharded Moon ingest: why Lunaris is single-shard-only, and what 0.7.0 should do about it

- **Status:** Draft — decision requested
- **Date:** 2026-08-15
- **Scope:** 0.7.0 blocker-check. Investigation only; no production code changed by this RFC.
- **Supersedes prose in:** `docs/operations/backup-restore.md` §6.6, `docs/operations/external-moon.md` §5
- **Evidence base:** vendor/moon source read at the pinned submodule checkout, plus a live
  probe against an ephemeral Moon **v0.8.1** (`--shards 1 / 2 / 4`, port 6394, destroyed
  after the run). Every behavioural claim below is either a `file:line` citation or a
  transcript from that probe.

> **Numbering note.** The task named this file `0007-sharded-moon-ingest.md`, but
> `docs/rfcs/0007-fallback-combinators.md` already exists. Writing 0007 would have
> clobbered a live RFC, so this is **0008**. Renumber if the orchestrator prefers.

---

## 0. Executive summary

Today's recorded position — "a sharded Moon is not a Lunaris backend"
(`docs/operations/external-moon.md:290`) — is **correct, but for a narrower reason than
recorded, and it under-states the problem on the read side.**

Three findings drive the recommendation:

1. **Hash tags alone do not fix ingest.** The `TXN.*` restriction is not "all keys must
   co-locate"; it is "all keys must land on **the connection's own shard**". Because Moon
   assigns connections to shards round-robin with no client control and no way to even
   query the shard count, hash-tagging the keyspace leaves ingest failing on roughly
   `1 - 1/N` of connections. Measured, not inferred.
2. **`MULTI/EXEC` does fix it** — it routes a co-located body to the *owner* shard and
   executes it there, so the connection's shard stops mattering. A realistic 7-op Lunaris
   ingest body succeeds under `MULTI/EXEC` + `{scope}` tags on a 4-shard Moon where the
   identical body under `TXN.*` fails. This makes the write-side fix far cheaper than
   assumed — but it costs `TXN.ABORT`, and Lunaris' graph-create path depends on reading a
   reply mid-transaction, which `MULTI` cannot provide.
3. **The read side has an independent, unfixed blocker.** `FT.NAVIGATE` — which
   `lunaris-storage-moon/src/navigate.rs:47` issues on the recall path — does **not**
   scatter-gather. Moon runs it against the connection's own shard only
   (`vendor/moon/src/server/conn/handler_monoio/ft.rs:540-546`, a bare `with_shard`). Fixing
   ingest would produce a deployment that *writes* correctly and *silently returns
   no graph hits* for every scope not on the connection's shard. Plain
   `FT.SEARCH … FILTER` is likewise rejected outright under multi-shard.

**Recommendation: Option C — document single-shard as the supported shape for 0.7.0, and
harden the guardrails.** The write-side fix is now known to be cheap, but it buys nothing
until the read side scatters, and no benchmark shows Lunaris needs more than one shard.
Sharding is a v0.8+ topic gated on Moon-side `FT.NAVIGATE` scatter-gather. Details and
effort estimates in §6.

---

## 1. Question 1 — Moon's shard-slot semantics

### 1.1 Slot computation: xxhash64 over the hash tag, else the whole key

```rust
// vendor/moon/src/shard/dispatch.rs:169-173
pub fn key_to_shard(key: &[u8], num_shards: usize) -> usize {
    let hash_input = extract_hash_tag(key).unwrap_or(key);
    (xxh64(hash_input, HASH_SEED) % num_shards as u64) as usize
}
```

`extract_hash_tag` (`dispatch.rs:192-199`) takes the content between the first `{` and the
first following `}`; an empty `{}` is ignored, matching Redis Cluster. `HASH_SEED = 0`
(`dispatch.rs:162`). There is **no** 16384-slot indirection on this path — the modulo is
taken directly against `num_shards`. (`src/cluster/slots.rs:20` computes a Redis-compatible
CRC-style slot from the same tag, but that is for `CLUSTER` introspection, which is disabled
by default: the probe returned `ERR This instance has cluster support disabled`.)

### 1.2 FT.* indexes: definitions are global, contents are per-shard, reads scatter

- **`FT.CREATE` is broadcast to every shard.** `vendor/moon/src/shard/coordinator.rs:2070-2073`:
  *"Broadcast an FT.\* command (FT.CREATE, FT.DROPINDEX) to ALL shards. Each shard creates
  its own copy of the index so HSET auto-indexing works regardless of which shard the key
  routes to."* Implementation `broadcast_vector_command`, `coordinator.rs:2082-2167`.
- **Contents are partitioned by the owning shard of the document key.** `auto_index_hset` runs
  inside the *local* write path only (`handler_sharded/mod.rs:1625-1631`, monoio twin
  `handler_monoio/mod.rs:1636`), so an entry lands in the index partition of whichever shard
  `key_to_shard` selected.
- **`FT.SEARCH` scatter-gathers and merges.** Vector KNN: `scatter_vector_search_remote`
  (`coordinator.rs:2001-2068`) → `merge_search_results`
  (`src/command/vector_search/ft_search/response.rs:92-144`). Text/BM25 runs a two-phase
  distributed-frequency protocol — Phase 1 aggregates per-term `df` and `N` across shards
  (`coordinator.rs:2430-2490`), Phase 2 re-scatters with the global IDF injected
  (`coordinator.rs:2492-2561`). Hybrid: `src/shard/scatter_hybrid.rs:63`.

**Live confirmation (probe, `--shards 4`).** Two per-scope text indexes were created, one
whose docs hash-tag onto the connection's shard (`{s0}`) and one onto a foreign shard
(`{s1}`). Both answered identically:

```
FT.SEARCH idx_s0 alpha  -> 2 hits, __bm25_score 0.182322 each
FT.SEARCH idx_s1 alpha  -> 2 hits, __bm25_score 0.182322 each   # foreign shard, correct
```

**So the answer to the sub-question is: co-locating a scope's KV by `{scope}` does NOT
break cross-scope `FT.SEARCH`.** That path is genuinely shard-transparent.

### 1.3 …but four FT.* commands do *not* scatter — and Lunaris uses one of them

This is the finding that changes the decision.

```rust
// vendor/moon/src/server/conn/handler_monoio/ft.rs:540-546
// FT.RECOMMEND, FT.NAVIGATE, FT.EXPAND need db/graph — dispatch locally
if cmd.eq_ignore_ascii_case(b"FT.RECOMMEND")
    || cmd.eq_ignore_ascii_case(b"FT.NAVIGATE")
    || cmd.eq_ignore_ascii_case(b"FT.EXPAND")
{
    let db_index = conn.selected_db as u8;
    let response = crate::shard::slice::with_shard(|s| { … });   // ← connection's shard ONLY
```

`with_shard` is the connection's own thread-local slice. There is no fan-out, no merge. On
the tokio runtime these fall into the catch-all `broadcast_vector_command`
(`handler_sharded/ft.rs:303-314`), which executes everywhere but **returns only the
coordinator's local answer** (`coordinator.rs:2166`) — same outcome.

Lunaris issues `FT.NAVIGATE` on the recall path:
`crates/lunaris-storage-moon/src/navigate.rs:47`. On a multi-shard Moon, a Navigate for a
scope whose data sits on shard 2, issued from a connection on shard 0, returns **empty — no
error**. That is silent recall degradation, precisely the failure mode Lunaris' core value
proposition ("a graph that's opt-in", sub-25ms recall) cannot tolerate.

Additionally, plain vector `FT.SEARCH … FILTER` is **rejected outright** under multi-shard:
`ERR FILTER not supported in multi-shard mode yet` (`handler_sharded/ft.rs:226-229`,
`handler_monoio/ft.rs:400-402`). HYBRID `FILTER` *is* supported (threaded per-shard via
`FtHybridPayload.filter`, `dispatch.rs:287-292`).

One further nuance: BM25 globalises `df` and `N` but **not `avgdl`** — length normalisation
still uses each shard's local `stats.avg_doc_len()` (`src/text/store.rs:611`, `:889`). Benign
when a scope is fully co-located (its shard's avgdl *is* the scope's avgdl), but it means
BM25 scores are not strictly comparable across shards for a split scope.

### 1.4 Graph store: strictly shard-local, routed by graph NAME

```
// vendor/moon/src/graph/mod.rs:5-16
A named graph lives ENTIRELY on one shard — every GRAPH.* command routes by hashing
the graph name … There is NO cross-shard traversal … partition a workload by GRAPH,
not within one.
```

Routing is `graph_to_shard(name, num_shards)` = `key_to_shard(name, num_shards)`, i.e. the
same hash-tag-aware xxh64 (`dispatch.rs:183-187`). A `GRAPH.QUERY` arriving on the wrong
shard is transparently SPSC-forwarded to the owner and the reply relayed back
(`handler_monoio/write.rs:1029-1063`, tokio twin `handler_sharded/write.rs:812-865`).
Cross-shard hop cost is documented at ~10 µs (`src/config.rs:216-217`).

So graph **reads and ordinary writes** are shard-transparent. Only Cypher writes *inside a
TXN* are rejected (§4.2).

---

## 2. Question 2 — Key-format feasibility

### 2.1 Every key kind in one ingest TXN

`crates/lunaris-storage-moon/src/atomic.rs::run_ops` (`:219-477`) is the complete fan-out.
There are exactly five `WriteOp` variants and each maps to one Moon command:

| `WriteOp` | Moon command | Key / name written | Carries scope? |
|---|---|---|---|
| `KvPut` (`atomic.rs:226-232`) | `HSET <key> v <val>` | caller-supplied, `lunaris:{scope}:{kind}:{ulid}` | **yes** |
| `KvDelete` (`:233-235`) | `DEL <key>` | same | **yes** |
| `VectorUpsert` (`:236-294`) | `HSET <key> vec … meta … content …` | `{ft_index_name(scope,kind)}:{id_hex}` = `lunaris_{scope}_{kind}_idx:{hex}` | **yes** |
| `GraphNode` (`:295-416`) | `GRAPH.QUERY` (optimistic) + `GRAPH.ADDNODE` + `GRAPH.QUERY` (create follow-up) | `graph_key(scope)` = `lunaris_{scope}_graph` | **yes** |
| `GraphEdge` (`:417-464`) | `GRAPH.QUERY` | `graph_key(scope)` | **yes** |

Key formats: `lunaris_core::keyspace::scope_prefix` (`crates/lunaris-core/src/keyspace.rs:49-51`),
`ft_index_name` (`crates/lunaris-storage-moon/src/keyspace.rs:58-60`), `graph_key`
(`keyspace.rs:67-69`).

The concrete WriteOp inventory a single ingest emits:

- `lunaris-ingest/src/pipeline.rs:362-427` — doctree, episode, chunk KvPuts + chunk/summary VectorUpserts.
- `lunaris/src/ingest.rs:508-614` (graph-ON path) — episode, chunk, fact KvPuts; chunk/entity/fact VectorUpserts; GraphNode; GraphEdge.
- `lunaris/src/structured_ingest.rs:319-531` — same shape plus `fact_spo_key` KvPuts.

**Every key in every ingest TXN carries the scope segment. There is no scope-less key,
no global counter, no shared meta key.** The FT *index definition* (`FT.CREATE`) is issued
outside `atomic_write` (`lunaris-storage-moon/src/client.rs:717-790`) and is broadcast to all
shards anyway (§1.2), so it is not a co-location hazard.

Two post-commit commands run on the same connection but **outside** the TXN:
`TEMPORAL.SNAPSHOT_AT` (`atomic.rs:113-115`) and nothing else. Not a factor.

### 2.2 Would wrapping the scope as a hash tag co-locate the whole TXN? — Yes, provably

Adding braces around the scope segment makes the hash input identical across all three
name families:

| today | tagged | `extract_hash_tag` input |
|---|---|---|
| `lunaris:acme.agent-1:episode:01H…` | `lunaris:{acme.agent-1}:episode:01H…` | `acme.agent-1` |
| `lunaris_acme.agent-1_chunks_idx:aa` | `lunaris_{acme.agent-1}_chunks_idx:aa` | `acme.agent-1` |
| `lunaris_acme.agent-1_graph` | `lunaris_{acme.agent-1}_graph` | `acme.agent-1` |

**Live confirmation (`--shards 4`).** Sweeping 16 distinct tags through a 3-op tagged TXN
body, the rejection count per attempt was **always 0 or 3, never 1 or 2** — all-or-nothing,
i.e. the tag really does collapse every key onto one shard:

```
tag=s0 rejected=0    tag=s4 rejected=0    tag=s8  rejected=0    tag=s12 rejected=3
tag=s1 rejected=3    tag=s5 rejected=3    tag=s9  rejected=3    tag=s13 rejected=0
tag=s2 rejected=0    tag=s6 rejected=3    tag=s10 rejected=3    tag=s14 rejected=3
tag=s3 rejected=3    tag=s7 rejected=3    tag=s11 rejected=3    tag=s15 rejected=3
```

### 2.3 …but co-location is NOT sufficient for `TXN.*` — the killer finding

Note what the sweep above actually shows: **5 of 16 tags succeed and 11 fail.** Co-location
is perfect, yet most tags still fail. The reason is that the guard is not "do the keys
agree with each other" but "does the key's owner shard equal **this connection's** shard":

```rust
// vendor/moon/src/server/conn/handler_monoio/mod.rs:2063-2069
} else if let Some(target) = target_shard {
    // TXN cross-shard guard: reject cross-shard writes in active TXN (no undo log).
    if conn.in_cross_txn() && metadata::is_write(cmd) {
        responses.push(Frame::Error(… ERR_TXN_CROSS_SHARD));
        continue;
    }
```

The `else if` branch is reached only when `is_local` is false — i.e. `target_shard !=
ctx.shard_id`. Every attempt in the sweep above ran on a fresh connection, and the pattern
was **byte-identical across two full re-runs**, so the connection consistently landed on one
shard and only the ~5 tags hashing to *that* shard could transact.

**And the client cannot choose, or even discover, its shard.** The central listener assigns
connections round-robin:

```rust
// vendor/moon/src/server/listener.rs:453-459  (twin at :584-590)
let s = next_shard;
next_shard = (next_shard + 1) % num_shards;
```

The probe further confirmed there is no introspection escape hatch: `CLUSTER KEYSLOT` →
`ERR This instance has cluster support disabled`; `INFO` (all sections) and
`CONFIG GET *shard*` expose **no shard count**; `CLIENT INFO` exposes no shard id.

> **Consequence.** Under round-robin accept, the *same* scope's ingest would succeed or
> fail depending on which connection the pool handed out — non-deterministic, not merely
> degraded. Hash-tagging the keyspace, on its own, does not make `TXN.*` usable.
> This is the single most important correction to the position recorded in
> `docs/operations/external-moon.md:290-305`, which implies hash tags are the fix.

### 2.4 Reproduction of today's failure, and the single-shard control

Today's exact key format, 7 ops, `--shards 4` (`probe1`):

```
TXN BEGIN                                                    -> OK
HSET lunaris:acme.agent-1:episode:…    -> ERR TXN does not support cross-shard writes
HSET lunaris:acme.agent-1:chunk:…      -> 1                 (this one happened to be local)
HSET lunaris:acme.agent-1:fact:…       -> ERR …cross-shard…
HSET lunaris:acme.agent-1:entity:…     -> ERR …cross-shard…
HSET lunaris:acme.agent-1:relation:…   -> 1                 (local)
HSET lunaris_acme.agent-1_chunks_idx:… -> ERR …cross-shard…
HSET lunaris_acme.agent-1_facts_idx:…  -> ERR …cross-shard…
TXN COMMIT                                                   -> OK      ← note!
```

Identical body at `--shards 1`: all seven return `1`, `TXN COMMIT` → `OK`.

**Incidental hazard worth recording.** `TXN COMMIT` returned `OK` having committed the two
ops that happened to be local while five were rejected — a **partial commit at the protocol
level**. Lunaris is safe here only because `run_ops` propagates the first per-op error and
short-circuits to `TXN.ABORT` (`atomic.rs:93-97`); any client that ignored per-op errors
would silently persist a torn episode. Worth an upstream note.

### 2.5 The write-side fix that *does* work: `MULTI/EXEC`

`MULTI/EXEC` classifies the whole queued body up front via `analyze_txn_locality`
(`vendor/moon/src/server/conn/shared.rs:800-874`), which uses the same hash-tag-aware
`key_to_shard` **and** treats a `GRAPH.*` name as a pseudo-key (`shared.rs:859-868`). When
the body resolves to a single owner, `execute_txn_on_owner`
(`src/shard/coordinator.rs:249-267`) ships `ShardMessage::TxnExecute` and the owner runs the
whole body on its slice (`src/shard/spsc_handler.rs:2695-2772`). Genuinely cross-shard
bodies are rejected `CROSSSLOT` (`handler_monoio/write.rs:834-840`).

**This removes the connection-shard dependency entirely.** Live, `--shards 4`, same
realistic 7-op ingest body:

```
# untagged (today's format) under MULTI/EXEC
MULTI … 7×HSET … EXEC   -> CROSSSLOT Keys in MULTI/EXEC don't hash to the same shard
                           (and NOTHING was written — clean atomic reject)

# same body, {scope} hash-tagged
MULTI … 7×HSET … EXEC   -> 1 1 1 1 1 1 1
HGET lunaris:{acme.agent-1}:episode:01HZA1 v      -> "ep"
HGET lunaris_{acme.agent-1}_facts_idx:bb   vec    -> "y"
```

The tagged body succeeded **even though `{acme.agent-1}` does not own the connection's
shard** — owner-routing did its job. The same body under `TXN.*` failed on that connection.

Mixed KV + graph works too, and the tag agreement is enforced:

```
MULTI; HSET lunaris:{gx}:episode:E1 v ep; GRAPH.ADDNODE lunaris_{gx}_graph Person _key k1; EXEC
  -> 1, 4294967297                          # both executed

MULTI; HSET lunaris:{s0}:episode:E2 v ep;  GRAPH.ADDNODE lunaris_{gx}_graph Person _key k2; EXEC
  -> CROSSSLOT                              # KV tag ≠ graph-name tag, correctly refused
```

**Cost of switching to `MULTI/EXEC` (both real):**

1. **No rollback.** `EXEC` has Redis semantics — a failing command returns an error inside
   the reply array; earlier commands stay applied. `atomic_write`'s `TXN.ABORT` path
   (`atomic.rs:93-97`) has no equivalent. Mitigating context: per the TXN deep-dive, Moon's
   `XactCommit` WAL record is an explicit **no-op** at recovery
   (`src/persistence/recovery.rs:437-476`) because the individual writes already ride the
   per-command AOF path, so `TXN.*` does not actually buy crash-atomicity today either.
   `MULTI/EXEC` is meanwhile *stronger* on isolation (whole body runs synchronously on one
   shard thread, nothing interleaves).
2. **No mid-transaction replies.** `WriteOp::GraphNode`'s create path issues
   `GRAPH.ADDNODE`, reads the returned internal node id, and interpolates it into a
   follow-up Cypher `SET` (`atomic.rs:391-414`). Under `MULTI` every reply arrives only at
   `EXEC`, so this shape cannot survive as written. `atomic.rs:349-362` explains why
   `ADDNODE` is mandatory (it is the only writer that registers the `key_to_node` mapping
   `FT.NAVIGATE` needs) — so this is not a trivially droppable round trip. Restructuring is
   required: hoist the node-existence resolution before the `MULTI`, or get Moon's Cypher
   `CREATE`/`MERGE` to call `register_key`.

---

## 3. Question 3 — Migration cost of a key-format change

### 3.1 There is no rename path

`RENAME` is implemented purely against the **local** `Database`
(`vendor/moon/src/command/key.rs:818-845`: `db.remove(src)` then `db.set(dst, entry)`). A
rename whose destination hashes to a different shard is exactly the cross-shard two-key case
the coordinator refuses. And a `{scope}` tag *guarantees* the destination moves shard for
most keys — that is the entire point of the change. So `SCAN` + `RENAME` is not available
even in principle on a multi-shard target.

It also would not be sufficient on a single shard:

- **FT index membership does not follow a rename.** The index `PREFIX` changes
  (`lunaris_{scope}_chunks_idx` vs `lunaris_scope_chunks_idx`), so renamed hashes match no
  index. The docs must be re-`HSET` under a freshly `FT.CREATE`d index to be re-indexed
  (`auto_index_hset` fires on write, `spsc_handler.rs:3353-3366`).
- **Graphs have no rename at all.** `graph_key(scope)` changes; the old named graph would
  have to be replayed node-by-node into the new one.
- **Embeddings are not stored in KV.** `skip_serializing` keeps vectors out of the primitive
  KV blobs, so re-indexing means re-writing the FT rows from the vectors, which live only in
  the FT hashes — recoverable by copy, but a full walk regardless.

**Conclusion: it is a dump-and-reload, not a rename.**

### 3.2 …which makes it approximately free if it rides the 0.7.0 migration tool

0.7.0 already plans a PG+SQLite → Moon migration tool that re-reads every primitive and
re-writes it through the Moon backend. That tool necessarily calls
`lunaris_core::keyspace::*` and `lunaris_storage_moon::keyspace::*` to mint destination
keys. If the brace is added to those five helpers, the migration tool emits the new format
with **zero additional migration code** — the key change becomes a property of the writer,
not a separate pass.

The marginal cost is then only:

- the helper edits themselves (5 `format!` strings, ~10 lines),
- the `Scope` alphabet guarantee, which already helps us: `[A-Za-z0-9_\-.]{1,128}`
  (`CLAUDE.md`, enforced in `crates/lunaris-core/src/scope.rs`) contains **neither `{` nor
  `}`**, so a scope can never forge or truncate its own tag. This is a genuinely clean
  substitution — no escaping needed;
- `parse_scope_from_key` (`crates/lunaris-core/src/keyspace.rs:316-332`) and
  `vector::decode_key` must learn the braces, plus the ~30 format-pinning tests;
- **an in-place Moon→Moon re-key for existing deployments**, which the PG/SQLite tool does
  *not* cover. That is the one genuinely new piece: a `SCAN` + read + re-write + delete-old
  pass. `list_scopes` already walks `SCAN MATCH lunaris:*`
  (`crates/lunaris-storage-moon/src/scopes.rs:53-60`), so the enumeration primitive exists.

**Estimate: ~1 day if bundled into the 0.7.0 migration tool; ~3-4 days standalone** (the
delta being the Moon→Moon re-key pass, its idempotency/resumability, and a dual-read
compatibility window).

---

## 4. Question 4 — How hard is cross-shard TXN upstream?

**Verdict: for the KV leg it is a deep architectural constraint, not a policy check. For the
graph leg it is already half-lifted.**

### 4.1 The KV leg: four independent per-shard subsystems

The rejection stands in for a real inability, on four counts:

1. **The transaction's identity is per-shard.** `txn_id` and `snapshot_lsn` are minted from
   the vector MVCC `TransactionManager`'s per-shard `next_lsn`
   (`vendor/moon/src/vector/mvcc/manager.rs:45-75`; borrowed at
   `handler_monoio/txn.rs:34-40`). Two shards mint colliding `txn_id`s. A global or
   shard-tagged identity space is prerequisite to everything else.
2. **The undo log's replay target is a `!Send` thread-local.** `CrossStoreTxn`
   (`src/transaction/mod.rs:95-119`) hangs off `ConnectionState.active_cross_txn`
   (`src/server/conn/core.rs:258`); its `UndoLog` (`src/transaction/undo_log.rs:13-29`) is
   captured inside a `with_shard` closure (`handler_monoio/mod.rs:1583-1620`) and replayed
   via `with_shard_db` (`src/transaction/abort.rs:112-126`). `ShardSlice`
   (`src/shard/slice.rs:63-120`) carries `_not_send: PhantomData<Rc<()>>` at line 119 —
   touching another shard's slice **does not typecheck**.
3. **MVCC intents are per-shard.** `KvWriteIntents` (`src/transaction/kv_mvcc.rs:26-29`)
   and the `committed: RoaringTreemap` (`manager.rs:57`) are slice-local, so a remote write
   would be invisible to the visibility filter and its before-image unreachable at abort.
4. **The WAL is strictly per-shard.** `ShardDatabases.wal_append_txs: Vec<OnceLock<…>>`
   indexed by shard (`src/shard/shared_databases.rs:59`, routed at `:171`);
   `TXN.COMMIT` appends its single `XactCommit` to `ctx.shard_id` only
   (`handler_monoio/txn.rs:106`). There is **no prepare record type** in `WalRecordType` and
   no participant list in the `XactCommit` payload
   (`src/persistence/wal_v3/record.rs:409-450`) — so a cross-shard TXN needs either genuine
   2PC across N logs or a new shared log. Neither exists.

**Characterisation: two-phase commit across shard slices, not single-shard-lock.** The
single-shard-lock shape is architecturally excluded — the slices are `!Send`, so there is no
lock that spans them; all cross-shard interaction is message-passing over SPSC.

The nearest existing template is `abort_cross_store_txn_routed`
(`src/transaction/abort.rs:419-528`), which partitions graph undo by owner and ships
`ShardMessage::GraphRollback` per shard, awaiting acks. That covers items 2-3 for graphs. A
KV equivalent plus items 1 and 4 are new subsystems. **Estimate: multi-week Moon-side
project, cross-repo, with a recovery-path redesign.**

### 4.2 The graph leg is already cross-shard-capable inside a TXN

Contrary to the guard's appearance, `GRAPH.ADDNODE`/`ADDEDGE` are **deliberately permitted**
cross-shard inside a TXN. The guard names only `GRAPH.QUERY`:

```rust
// vendor/moon/src/server/conn/handler_monoio/write.rs:1036-1044
if conn.in_cross_txn()
    && cmd.eq_ignore_ascii_case(b"GRAPH.QUERY")
    && crate::command::graph::is_cypher_write_query(cmd_args)
```

…and immediately below (`write.rs:1065-1076`) the routed reply's integer id is captured as a
rollback intent via `txn.record_graph(…)`, which `abort_cross_store_txn_routed` then ships
back to the owner. The asymmetry is principled: `ADDNODE`'s undo is derivable from the reply
(one id + graph name), whereas a Cypher write's undo is a `Vec<GraphUndoOp>` produced inside
the owner's executor and `ShardMessage::GraphCommand` has no channel to carry it home.

**Live confirmation.** On `--shards 4`, `GRAPH.ADDNODE` against a foreign-shard graph inside
a TXN was accepted (returned node id `4294967298`) where the equivalent `GRAPH.QUERY` write
was rejected; a subsequent `TXN.ABORT` rolled it back (`MATCH (n:Hole) RETURN count(n)` →
`0`, against a baseline of `1` for the same `ADDNODE` outside a TXN). So this is working as
designed, **not** a corruption hole. My initial read of it as a guard hole was wrong.

### 4.3 Two genuine upstream defects found in passing

Neither blocks 0.7.0 (both are invisible at `--shards 1` or unreachable from Lunaris' code
paths today), but both should go upstream:

1. **`MSET` / multi-key `DEL` inside a TXN bypass undo capture with no error.**
   `try_handle_cross_shard_commands` (`handler_monoio/mod.rs:1220`) runs *before* both the
   undo-capture write path (`:1583`) and the generic guard (`:2064`), and
   `coordinate_multi_key` has no TXN awareness. `TXN.BEGIN; MSET a 1 b 2; TXN.ABORT` leaves
   both keys written — **even when both are local**, since the multi-key path is chosen on
   key count, not locality. Lunaris does not currently emit `MSET` or multi-key `DEL` inside
   `atomic_write`, so we are not exposed; a future batching optimisation would be.
2. **Abort-vs-recovery model contradiction.** `src/transaction/abort.rs:56-60` asserts an
   aborted TXN's forward records are never in the log; `src/persistence/recovery.rs:449-457`
   states the individual writes ride the AOF independently. If `recovery.rs` is right, an
   aborted TXN's writes survive restart. Not empirically tested here.

Related, and directly relevant to how we describe Lunaris: **`TXN.*` does not currently
provide crash-atomicity.** `XactCommit` is a counted no-op at recovery
(`recovery.rs:437-476`) and the per-command AOF append is gated only on `!is_error &&
is_write` with no `in_cross_txn()` check (`handler_monoio/mod.rs:1818-1868`), so an
in-flight TXN's writes are already in the AOF and the replication stream before `COMMIT`.
Lunaris' "provable atomicity" claim rests on in-memory `TXN.ABORT`, not on durable
transaction framing. That is a separate correctness question worth its own ticket.

---

## 5. Question 5 — Do we even need shards?

**No evidence that we do, and clear evidence that shards cost more than they give for
Lunaris' access pattern.**

### 5.1 Moon's own guidance is "use one shard"

- `vendor/moon/src/config.rs:222` — `#[arg(long, default_value_t = 1)] pub shards: usize`.
  **One shard is the binary default.**
- `vendor/moon/CLAUDE.md` (Gotchas) — *"Single-shard gives best throughput for
  non-pipelined workloads. Adding shards causes sub-linear scaling because most keys route
  cross-shard (SPSC dispatch overhead dominates local DashTable lookup). Use `--shards 1`
  unless testing pipeline/AOF benefits."*
- `vendor/moon/BENCHMARK.md:266` — *"Scaling 1→8 shards is flat-to-slightly-negative for
  uniform single-key GET/SET at c=50."* The §4.4 table (`BENCHMARK.md:436-442`) measures
  1→1.00×, 2→1.27×, 4→1.43×, 8→1.46×, **12→1.39×** (negative from 8 to 12).
- `vendor/moon/docs/guides/tuning.md:28-42` — `--shards 1` for 1-4 unpipelined connections;
  *"1 connection on many shards | avoid | pays the hop on ~every op (0.85-0.99×)"*.
- Counter-evidence, fairly stated: `BENCHMARK.md:580-598` reports the s4 multi-connection
  collapse was a reply-spin convoy bug now fixed; at 8+ concurrent connections `--shards 4`
  now beats Redis 1.5-2.5× unpipelined. So shards help **connection-concurrent** workloads.

### 5.2 Single-shard headroom vs Lunaris' target load

Measured, `--shards 1` (GCE `c3-standard-8` x86 / `t2a-standard-8` ARM, Ubuntu 24.04,
`--appendonly no`, `redis-benchmark -c 50 -n 400000`; conditions `BENCHMARK.md:216`):

| metric | x86 | ARM | citation |
|---|---|---|---|
| SET p=16 / p=64 (loose) | 1.90M / 3.08M ops/s | 1.40M / 2.52M | `BENCHMARK.md:259` |
| SET p=64 / p=16 / p=1 (**strict**, `-r 1000000`) | 930K / 766K / 107K | 835K / 611K / 73K | `BENCHMARK.md:238-240` |
| GET p=16 / p=64 | 1.59M / 4.71M | 1.01M / 3.60M | `BENCHMARK.md:259,262` |
| SET p=64, AOF `everysec` | 605K (0.36× Redis) | 591K (0.48×) | `BENCHMARK.md:250-253` |
| SET p=1, AOF `everysec` | 133K (0.97×) | 94K (0.90×) | `BENCHMARK.md:250-253` |

The strict figures are the honest ones — `BENCHMARK.md:18` warns loose and strict differ
3-4×. Note `BENCHMARK.md:253` explicitly: *"The per-shard-WAL advantage over Redis's single
AOF materializes with **more shards**, not at shards=1."*

**Against Lunaris' load.** An ingest is one `atomic_write` of ~7-20 commands, gated behind
LLM extraction that costs hundreds of milliseconds to seconds. Even at 73K writes/s
(the worst measured single-shard unpipelined SET number), a single shard absorbs ~3,600
ingests/second of fan-out — orders of magnitude beyond what the extractor can feed it.
**Write throughput is not, and will not be, the binding constraint.**

**Recall** is served by the FT index, not by TXN. The measured single-shard latency floor is
`BENCHMARK.md:311-314`: single-connection GET at 12.9 µs/op (ARM) / 15.8 µs (x86) with
busy-poll — three orders of magnitude under the 25 ms recall budget. Vector search at scale
is only benchmarked at **8 shards** (`BENCHMARK.md:881-1008`, 1.18M × 200d glove: ef=64 →
recall 0.827 @ 888 QPS single-client), so single-shard 1M-scale vector QPS is
**not documented** — the honest gap in this analysis (§7).

### 5.3 Where single-shard genuinely bites

Two real limits, both worth documenting rather than solving by sharding:

- **Cold-reload stall is shard-wide.** `vendor/moon/docs/guides/tuning.md:361-368` — a
  first-touch `FT.SEARCH` after cold reload took **79.59 ms**, and a concurrent `PING` on a
  *second connection to the same shard* peaked at **76.70 ms**. `tuning.md:344` notes that at
  `--shards 1` this means every connection on the server. That is a p99 recall-SLO event.
- **Bulk vector load throttling.** `BENCHMARK.md:997-1005` — the MA1 write-stall guard
  throttles 1M+ bulk load to **~190 vec/s (24× slowdown)** unless
  `--max-unflushed-immutable-segments 0`. Relevant to the 0.7.0 migration tool.
- **Persistence caveat.** `vendor/moon/docs/guides/persistence.md:63-67` — with
  `--appendonly no`, only *cross-shard* writes reach the WAL, so at `--shards 1` a crash
  loses everything. Every peak benchmark above runs `--appendonly no`; production must not.

---

## 6. Options

### Option A — Hash-tag key migration + `MULTI/EXEC`

Add `{}` around the scope in `scope_prefix`, `ft_index_name`, `graph_key`; switch
`atomic_write` from `TXN.*` to `MULTI/EXEC` so owner-routing removes the connection-shard
dependency; restructure `WriteOp::GraphNode`'s create path to avoid needing `ADDNODE`'s
reply mid-body.

- **Effort: ~2-3 weeks.** Key helpers + parsers + ~30 format tests (~2 days); `atomic_write`
  rewrite to `MULTI/EXEC` with red/green coverage (~3 days); `GraphNode` create-path
  restructure, the hard part, possibly needing a Moon-side `register_key`-on-`MERGE` change
  (~1 week, cross-repo risk); Moon→Moon re-key migration pass (~3 days); multi-shard
  integration suite (~3 days).
- **Risk: HIGH — and it does not deliver a working multi-shard backend.** `FT.NAVIGATE`,
  `FT.EXPAND`, `FT.RECOMMEND` still answer from the connection's shard only (§1.3), so
  recall silently loses graph hits for most scopes. Plain `FT.SEARCH … FILTER` is still
  rejected under multi-shard. Losing `TXN.ABORT` means a mid-`EXEC` failure leaves partial
  state with no rollback (§2.5).
- **Verdict: premature.** Correct direction, wrong order — it fixes writes into a store
  whose reads are broken.

### Option B — Upstream cross-shard `TXN.*` in Moon

- **Effort: multi-week Moon-side project, cross-repo.** Requires global transaction
  identity, remote intent registration, remote undo capture, and a 2PC protocol across N
  per-shard WALs including a recovery-side resolver (§4.1). The routed-graph-rollback
  precedent (`abort.rs:419-528`) covers perhaps a third of it.
- **Risk: VERY HIGH.** Touches Moon's recovery path — the subsystem where the two defects
  in §4.3 already indicate the model is not fully settled.
- **Verdict: reject.** `MULTI/EXEC` already provides single-owner routed atomicity (§2.5),
  which is everything Lunaris' write-only fan-out actually needs. Building 2PC to preserve
  an interactive-transaction API we do not require is not justified.

### Option C — Document single-shard, defer sharding (recommended)

Keep `--shards 1` as the supported and only shape for 0.7.0; spend the budget on making
that contract impossible to violate by accident.

- **Effort: ~1-2 days.**
  - Fold this RFC's corrected mechanism into `docs/operations/external-moon.md` §5 and
    `docs/operations/backup-restore.md` §6.6 — specifically, replace the implication that
    hash tags are the fix (§2.3) and add the `FT.NAVIGATE` read-side blocker (§1.3).
  - **Fail fast at startup**: have `MoonStorage::connect` detect a multi-shard server and
    refuse with a pointed error, instead of letting the first ingest fail mid-flight. Cheapest
    reliable probe today is a two-key co-location canary in a reserved scope, since Moon
    exposes no shard count (§2.3) — which is also the natural upstream ask (below).
  - **Fix the Docker footgun.** `vendor/moon/Dockerfile:149` runs `--shards 0` =
    auto-detect = sharded (`docs/operations/external-moon.md:117-120,308-310`). Moon's
    *binary* default is `--shards 1` (`config.rs:222`); only the image inverts it. Our
    published compose/helm examples must pin `--shards 1` explicitly.
- **Risk: LOW.** Documents reality; no behaviour change; no migration.
- **What we give up:** nothing measurable. §5.2 shows single-shard write headroom exceeds
  extractor-gated ingest by orders of magnitude, and recall is FT-bound, not TXN-bound.

**Upstream asks to open against Moon (small, high-leverage, unblock Option A later):**

1. Expose `num_shards` (e.g. an `INFO Server` field) so clients can detect and refuse
   multi-shard deployments cleanly.
2. Scatter-gather `FT.NAVIGATE` / `FT.EXPAND` / `FT.RECOMMEND`, or make them error loudly
   under `num_shards > 1` instead of returning empty. **This is the true gate on sharding.**
3. `TXN.COMMIT` should fail, not return `OK`, when any op in the transaction was rejected
   (§2.4).
4. The `MSET`/multi-key-`DEL` undo bypass and the abort-vs-recovery contradiction (§4.3).

---

## 7. Recommendation

**Adopt Option C.**

The investigation changed the shape of the problem twice. It is *not* true that hash tags
fix ingest (§2.3) — the recorded position overstates the fix. It is *also* not true that
ingest is expensive to fix: `MULTI/EXEC` + `{scope}` tags demonstrably works on a 4-shard
Moon, today, including mixed KV+graph bodies (§2.5). But fixing ingest yields a deployment
that writes correctly and **silently drops graph recall** for every scope not on the caller's
shard, because `FT.NAVIGATE` never scatters (§1.3). Shipping that would be worse than
today's loud failure.

Given that no measured workload needs a second shard (§5.2) and Moon's own guidance is
`--shards 1` (§5.1), the correct 0.7.0 move is to make the single-shard contract
unmissable and enforced, and to gate any future sharding work on upstream ask #2.

**Revisit when:** (a) Moon scatter-gathers `FT.NAVIGATE`, **and** (b) a measured Lunaris
workload saturates a single shard — most plausibly on connection concurrency
(`BENCHMARK.md:580-598`), not on throughput. Until both hold, Option A is effort spent
toward a backend we cannot correctly read from.

---

## Appendix A — Probe reproduction

Ephemeral Moon, destroyed after the run. Never on 6381 (live store) or 6399 (bench).

```bash
BIN=vendor/moon/target/release/moon          # Mach-O arm64, moon_version 0.8.1
DIR=$(mktemp -d)
$BIN --port 6394 --shards 4 --dir "$DIR" --appendonly no --disk-free-min-pct 1 &

# 1. today's key format fails, partial-commits, and TXN COMMIT still says OK
printf 'TXN BEGIN\nHSET lunaris:acme.agent-1:episode:A v 1\nHSET lunaris:acme.agent-1:chunk:B v 2\nTXN COMMIT\n' | redis-cli -p 6394

# 2. hash tags co-locate but TXN still needs the CONNECTION's shard (0-or-3, ~1/N tags pass)
for t in s0 s1 s2 s3; do
  printf 'TXN BEGIN\nHSET lunaris:{%s}:episode:A v 1\nHSET lunaris:{%s}:chunk:B v 2\nHSET lunaris_{%s}_chunks_idx:C vec 3\nTXN COMMIT\n' $t $t $t \
    | redis-cli -p 6394 | grep -c cross-shard
done

# 3. MULTI/EXEC owner-routes: same tagged body succeeds regardless of connection shard
printf 'MULTI\nHSET lunaris:{s1}:episode:A v 1\nHSET lunaris_{s1}_chunks_idx:C vec 3\nEXEC\n' | redis-cli -p 6394

# 4. MULTI/EXEC atomically refuses a genuinely cross-shard body (nothing written)
printf 'MULTI\nHSET {s0}:a v 1\nHSET {s1}:b v 2\nEXEC\n' | redis-cli -p 6394

# 5. FT.SEARCH is shard-transparent; per-scope index on a foreign shard answers correctly
redis-cli -p 6394 FT.CREATE idx_s1 ON HASH PREFIX 1 'doc_{s1}_' SCHEMA content TEXT
redis-cli -p 6394 HSET 'doc_{s1}_1' content "alpha bravo"
redis-cli -p 6394 FT.SEARCH idx_s1 alpha

pkill -f "moon --port 6394"; rm -rf "$DIR"
```

**Environment caveat.** The probe ran on darwin-arm64, where every fresh connection landed
on the same shard (two full 16-tag sweeps were byte-identical). On Linux the central
listener assigns round-robin (`listener.rs:453-459`) or the kernel assigns via per-shard
`SO_REUSEPORT` (`listener.rs:418`), so the landing shard varies per connection — which makes
`TXN.*` failure **non-deterministic per connection** there rather than stable. That
strengthens, not weakens, §2.3.

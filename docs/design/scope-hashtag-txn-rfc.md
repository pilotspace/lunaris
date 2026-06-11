# RFC: Moon TXN pinning + Lunaris `{scope}` hash-tag keyspace

- Status: **Proposed** (Lunaris-side design artifact; the mechanism is Moon work)
- Author: Lunaris team · 2026-06-11
- ADD task: `scope-hashtag-spike` (milestone `moon-v030-exploit`)
- Probe: `scripts/spike-scope-hashtag-probe.py` (exit 0 = evidence reproduces)
- Audience: Moon maintainers (Mechanisms / Recommendation) + Lunaris maintainers
  (Adoption / Migration)

## 1 · Problem

Lunaris' core contract (INGEST-04) is **one `atomic_write` per ingest**: every
episode's KV primitives, FT vector docs, and graph ops commit or roll back as a
unit, implemented over Moon `TXN BEGIN … TXN COMMIT/ABORT`.

Moon TXN is **shard-local**: the undo log lives in the per-connection
`CrossStoreTxn` and only captures writes executed on the shard that *accepted
the connection*. A keyed write inside a TXN that routes to another shard is
rejected with `ERR_TXN_CROSS_SHARD` — loud, never corrupting, but fatal to the
ingest.

Lunaris keys (`lunaris:<scope>:<kind>:<ulid>`) carry no hash tags, so on
`--shards N>1` a multi-key `atomic_write` sprays across shards and **every
multi-shard Moon deployment makes Lunaris ingest fail**. Today the gap is
*latent*: every Lunaris deployment recipe runs `--shards 1` (which Moon's own
perf guidance prefers for non-pipelined workloads). But "Lunaris cannot run on
a sharded Moon, ever" is not a position we want baked in silently.

The obvious client-side fix — brace the scope so all of a scope's keys
co-locate (`lunaris:{acme}:fact:…`) — **does not work on Moon v0.3.0**, and
this RFC exists because the reason is structural, not a bug.

## 2 · Evidence

### 2.1 Live probe (macOS, Moon v0.3.0, `vendor/moon/target/release/moon`)

`python3 scripts/spike-scope-hashtag-probe.py` launches its own 4-shard and
1-shard servers and asserts this matrix (run of 2026-06-11):

| Probe | Setup | Observed | Meaning |
|---|---|---|---|
| P1 | shards=4, 8 unbraced keys in one TXN | 4 OK / 4 `ERR_TXN_CROSS_SHARD` (an earlier manual run: 1 OK / 7) | Unbraced multi-key TXN partially rejects; the OKs are keys that happened to hash to the accept shard |
| P2 | shards=4, 8 keys all braced `{acme.a1}`, 16 fresh connections | 16/16 connections: **8/8 rejected**, 0 lucky, per-connection homogeneous | Hash tags co-locate keys *with each other*, but the TXN binds to whatever shard **accepted the connection** (SO_REUSEPORT) — and a client cannot pick its shard |
| P3 | shards=1 control, 8 unbraced keys | 8/8 OK, `TXN COMMIT` → `+OK` | The gap is latent on every current single-shard recipe |

P2 is the finding that reshapes the design: **client-only hash-tagging is
insufficient**. On this macOS/kqueue run, not one of 16 connections landed on
the tag's shard — accept distribution is not something a client can steer or
even retry toward reliably.

### 2.2 Code pointers (vendor/moon @ v0.3.0)

The TXN machinery is shard-local at four distinct layers — any fix has to
account for all of them:

1. **State lives on the connection.** `ConnectionState.active_cross_txn:
   Option<CrossStoreTxn>` (`src/server/conn_state.rs`); `CrossStoreTxn` holds
   the `kv_undo` log and `mq_intents` (`src/transaction/mod.rs`). The undo log
   only sees writes the accept-shard handler executes locally.
2. **The guard.** `src/server/conn/handler_sharded/mod.rs:1630`:
   `if conn.in_cross_txn() && metadata::is_write(cmd)` on the cross-shard
   dispatch path → `ERR_TXN_CROSS_SHARD` (`src/command/transaction.rs:36`).
   Sibling guard sites at `mod.rs:1153`, `mod.rs:1216`, `mod.rs:1535`,
   `dispatch.rs:474`, `write.rs:684`, with `handler_monoio/` twins.
3. **Commit touches four `ctx.shard_id`-bound structures**
   (`src/server/conn/handler_sharded/txn.rs`, `try_handle_txn_commit`):
   the vector store's `txn_manager` (begin/commit/abort + snapshot LSN), the
   per-shard **WAL** (`encode_xact_commit_payload` → `wal_append(ctx.shard_id)`),
   the **`kv_intents`** side-table (`release_txn`), and the deferred
   **`hnsw_queue`** (`drain_for_txn`). Abort mirrors this through
   `src/transaction/abort.rs`.
4. **Routing.** `slot_for_key` uses `extract_hash_tag` — first `{…}` substring
   if present, else the full key (`src/cluster/slots.rs:19-20`). So a braced
   format co-locates keys deterministically; only the *connection↔shard*
   binding is missing.

## 3 · Mechanisms (Moon-side)

Three ways to give a TXN a deterministic home shard. All keep the existing
invariant: a write whose slot is not the TXN's home shard is **rejected loudly**
(no distributed undo / 2PC anywhere in this RFC).

### M1 — `TXN BEGIN PIN <key>`: declared pin + shard-side TXN state + SPSC forwarding

The client declares its routing key at begin time. The server computes
`home = shard_of(slot_for_key(<key>))` and creates the TXN **on the home
shard** in a per-shard side table keyed by `txn_id` (moving `CrossStoreTxn`
out of `ConnectionState`; the connection keeps only `(txn_id, home_shard)`).
Subsequent TXN commands route:

- keyed write whose slot → home shard: forwarded over the **existing SPSC
  write-dispatch channel** (the same path that already carries cross-shard
  writes outside TXN), tagged with `txn_id` so the home shard's handler
  captures undo and registers intents locally;
- keyed write whose slot → any other shard: `-ERR TXN PIN mismatch — key
  routes to shard X, txn pinned to shard Y` (new error, same loud-failure
  philosophy as `ERR_TXN_CROSS_SHARD`);
- `TXN COMMIT` / `TXN ABORT`: forwarded to the home shard, which already owns
  all four commit-time structures (§2.2.3) — **no WAL or undo-log format
  change**, because every TXN write physically executed there.

Needs additionally: an abort-on-disconnect hook (today the TXN dies with the
`ConnectionState`; shard-side state needs the accept handler to enqueue an
abort when the conn drops, plus a TTL sweep reusing the existing MA2
`old_snapshot_threshold` kill machinery as backstop).

**Cost: medium.** State relocation (`ConnectionState` → per-shard
`HashMap<TxnId, CrossStoreTxn>`), SPSC frame gains a `txn_id` tag, both
`handler_sharded` and `handler_monoio` twins change, disconnect GC. No new
network layer, no WAL change, works identically on Linux and macOS and both
runtimes. Response ordering is already solved by the SPSC pipeline-batch
plumbing (pre-allocated response slots).

### M2 — Connection shard-pin handshake (`CONN PIN <key>` → migrate the connection)

Reuse Moon's **connection migration** (Linux/io_uring feature): on
`CONN PIN <key>` the accept shard hands the fd to the event loop of the shard
owning the key's slot. After migration, everything — including the existing
TXN code — is naturally shard-local; the TXN implementation does not change at
all.

**Cost: low on Linux, unavailable on macOS** (kqueue path has no connection
migration — `vendor/moon/CLAUDE.md` target-platform matrix), so dev/test
parity breaks: the exact environment where Lunaris developers run probes and
integration tests could not exercise the pinned path. Also per-scope pinning
makes connection pools scope-sticky, which fights Lunaris' shared-pool design
(one `MoonClient` multiplexes all scopes).

### M3 — Transparent server-side TXN forwarding (first write binds the home)

No client change: `TXN BEGIN` defers binding; the **first keyed write**'s slot
elects the home shard, then M1's forwarding takes over; later writes to other
shards are rejected as in M1. Strictly a superset of M1's machinery plus lazy
binding.

**Cost: highest for the least determinism.** The "no client change" promise is
illusory — clients must still brace their keys or the second write fails, so
they are already cooperating; meanwhile error reporting gets worse (the
mismatch error appears on write #2, not at `BEGIN`, and which shard "won"
depends on key order). Going further to true cross-shard atomicity means
distributed undo/2PC across per-shard WALs — explicitly out of scope.

## 4 · Recommendation

**M1 — `TXN BEGIN PIN <key>`.** It is deterministic (binding visible at
`BEGIN`), portable (both OSes, both runtimes), reuses the SPSC dispatch path
and all four shard-local commit structures unchanged, and fails loudly on
mismatch exactly like v0.3.0 does today. M2 is attractive on Linux but forks
behavior across platforms Lunaris actually develops on; M3 buys nothing a
braced client doesn't already have, at more cost.

Proposed command surface (additive, backward compatible):

```
TXN BEGIN                      -- unchanged: accept-shard-local TXN (v0.3.0 semantics)
TXN BEGIN PIN <key>            -- +OK; home shard = shard_of(slot_for_key(<key>))
<any write, slot == home>      -- forwarded + undo-captured on home shard
<any write, slot != home>      -- -ERR TXN PIN mismatch (key->shard X, txn pinned->Y)
TXN COMMIT | TXN ABORT         -- forwarded to home shard
```

Undo-log / WAL implications, by design **none in format**: all TXN writes
execute on the home shard, so `kv_undo`, `kv_intents`, the deferred
`hnsw_queue`, MQ intents, and the per-shard `XactCommit` WAL record all stay
exactly as they are. The two real changes are *where the state lives*
(per-shard table keyed by `txn_id` instead of `ConnectionState`) and *how
frames reach it* (SPSC tag + forwarded COMMIT/ABORT + disconnect-abort GC).

For Lunaris, `PIN` composes perfectly with a braced keyspace: every
`atomic_write` is single-scope, so the pin key is simply any key of the batch —
they all share the `{scope}` tag.

## 5 · Lunaris adoption — `{scope}`-braced keyspace v2

The Scope alphabet `[A-Za-z0-9_\-.]{1,128}` **excludes braces**, so wrapping
the scope is collision-free and reversible, and `extract_hash_tag` (first
`{…}`) lands exactly on it. Because RC-1 centralized every key mint in
`lunaris_core::keyspace`, the format flip is a **single-module change** — that
convention is what makes this migration cheap.

Key families (all of them, including the non-KV shapes):

| Family | v1 (today) | v2 (braced) |
|---|---|---|
| KV primitives (episode, chunk, entity, relation, fact, community, doctree) | `lunaris:<scope>:<kind>:<ulid>` | `lunaris:{<scope>}:<kind>:<ulid>` |
| FT doc keys | `lunaris_<scope>_<kind>_idx:<hex>` | `lunaris_{<scope>}_<kind>_idx:<hex>` (braces are literal bytes in the FT `PREFIX`, so index provisioning changes in lock-step) |
| Graph store | `lunaris_<scope>_graph` | `lunaris_{<scope>}_graph` |
| MQ topics + MCP scratchpad/working-memory keys | unscoped/`lunaris:`-prefixed | same brace rule wherever a scope appears |

Migration sketch (gated on Moon shipping `PIN`; do **not** migrate before — a
braced keyspace buys nothing on v0.3.0, per the probe):

1. **Flag-gated mint:** `keyspace_v2` feature/config in `lunaris_core::keyspace`
   emits braced keys; default off.
2. **Dual-read window:** point reads try v2 then fall back to v1
   (`hydrate`, graph lookups, FT searches run against both index families —
   FT needs both indexes provisioned during the window).
3. **Backfill or re-ingest:** for Moon, a `SCAN`-driven rename per family
   (`RENAME` is single-key, shard-safe per key) or plain re-ingest for small
   tenants; classify-and-copy order: KV → FT docs (re-`HSET` so auto-indexing
   re-fires) → graph (export/import or Cypher rebuild).
4. **Flip default, drop dual-read** after a full verify pass
   (`read_as_of` + recall parity per scope), then delete v1 indexes.
5. SQLite/Postgres backends are untouched (their isolation is row/RLS-based,
   not key-shape-based) — parity tests pin that the DSL surface is unchanged.

Rollback at any step = flip the flag back; dual-read keeps v1 authoritative
until step 4.

## 6 · Open question — Linux connection migration

This probe ran on **macOS/kqueue**, where connection migration does not exist.
On Linux/io_uring, Moon migrates connections between shard event loops for
load reasons — it is *possible* the accept-shard binding story differs (e.g. a
migrated connection's TXN state moving with it, or the M2 mechanism being
nearly free because the hand-off plumbing already exists). The probe cannot
falsify that from this host, so the RFC records it as open rather than
asserting.

Verification recipe (OrbStack, Moon repo conventions):

```bash
orb create ubuntu moon-dev   # if absent; see vendor/moon/CLAUDE.md for full setup
orb run -m moon-dev bash -c 'source ~/.cargo/env && \
  cd /Users/tindang/workspaces/tind-repo/moon && cargo build --release'
# run THIS probe inside the VM against the Linux binary:
orb run -m moon-dev bash -c 'cd /Volumes/Games/tindang-repo/lunaris && \
  python3 scripts/spike-scope-hashtag-probe.py --moon-bin \
  /Users/tindang/workspaces/tind-repo/moon/target/release/moon'
```

If P2 shows lucky-shard variance (some connections all-OK) the binding story
holds but is load-dependent; if it shows something qualitatively different
(e.g. braced TXNs succeeding), M2's cost estimate drops and this RFC's
recommendation should be revisited before scheduling M1.

## 7 · Follow-up tasks

1. **Lunaris v0.7 hardening (cheap, immediate):** connect-time shard-count
   guard in `lunaris-storage-moon` — probe `INFO`/`CONFIG GET` at
   `MoonClient::connect`, and **fail fast (or loudly warn) when `shards > 1`**,
   pointing at this RFC. Today the operator only finds out when the first
   multi-key ingest throws `ERR_TXN_CROSS_SHARD`.
2. **Moon:** schedule M1 (`TXN BEGIN PIN <key>`) in the Moon repo — state
   relocation, SPSC `txn_id` tag, forwarded COMMIT/ABORT, disconnect-abort GC,
   `monoio`/`sharded` twins, consistency-suite entries.
3. **Lunaris keyspace v2** (blocked on #2): flag-gated braced mint +
   dual-read + backfill per §5.
4. **Linux probe run** per §6 before #2 is finalized.

## Appendix — probe verdict (verbatim, 2026-06-11)

```
VERDICT — multi-shard TXN probe (Moon @ vendor/moon/target/release/moon )
----------------------------------------------------------------------------------------------------
P1 unbraced TXN, shards=4      MATCH (partial/full reject)   4 OK / 4 cross-shard-rejected of 8
P2 {tag}-braced TXN, shards=4  MATCH (binding proven)        16 conns: 0 all-OK (lucky shard), 16 all-rejected, homogeneous=True
P3 control, shards=1           MATCH (gap is latent)         8 OK / 0 rejected of 8, TXN.COMMIT=+OK
----------------------------------------------------------------------------------------------------
RESULT: MATCH — TXN is shard-local AND binds to the accept shard; client-side
hash-tagging alone cannot restore atomicity (see docs/design/scope-hashtag-txn-rfc.md).
```

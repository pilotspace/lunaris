# Durability & Recovery

**Lunaris is stateless: every byte of durable state lives in Moon, so
recovery is a Moon concern.** This chapter
documents the Moon-backed crash-recovery path, the bi-temporal MVCC
semantics the guarantees rest on, the two live-measurement gotchas you need
to know, and how to test recovery yourself.

> Adapted from `docs/durability.md` (kept in the repo as the canonical
> standalone version). Status: alpha. All claims validated against live Moon
> on 2026-04-23 — rerun `scripts/test-recovery.py` to re-verify.

## 1. What survives a crash

| entity | persisted in | survives Moon crash? | survives Lunaris client crash? |
|---|---|---|---|
| Episode body (KV) | Moon HSET at `lunaris:{scope}:episode:<ulid>` | yes — via AOF + RDB | n/a (client-side is stateless) |
| Chunk text (KV) | Moon HSET at `lunaris:{scope}:chunk:<ulid>` | yes | n/a |
| Vector (768-d) | Moon `chunks:<hex>` HSET + FT HNSW | yes | n/a |
| BM25 index | Moon FT inverted index | yes (rebuilt from HSETs on replay) | n/a |
| Graph (entities, facts) | Moon `GRAPH.*` storage | yes | n/a |
| Pipeline queue (consolidate, verify) | Moon Streams | yes | n/a |
| In-flight ingest that wasn't `await`-ed | — | no (never committed) | no (never committed) |

Every user-visible write goes through `atomic_write`
(`crates/lunaris-storage-moon/src/atomic.rs`). The ingest umbrella splits a
single `Episode` into one batch of `WriteOp` values (KV puts + vector upserts
+ optional graph writes) and ships them inside one atomic envelope. Moon
commits the envelope as a unit — partial envelopes never appear in the AOF.
This is the same single-`atomic_write`-per-ingest moat enforced by the CI
grep gate; see [Ingesting Observations](../guides/ingest.md).

## 2. Bi-temporal MVCC semantics

Every primitive (`Episode`, `Chunk`, `Entity`, `Fact`, `Relation`,
`Community`) carries a required `bt` field — a bi-temporal stamp
`{ valid: (Hlc, Option<Hlc>), sys: (Hlc, Option<Hlc>) }` (Snodgrass
bi-temporal *at the storage model*: *valid time* = when the fact was true in
the world, *system time* = when Lunaris knew it). Nothing is updated in
place:

> **Scope of the claim.** The write model below is fully bi-temporal, and
> as-of *reads* work on the search and graph lanes (`FT.SEARCH AS_OF`,
> `GRAPH.QUERY VALID_AT`). Historical **KV** reads do not: Moon stores
> Lunaris rows as plain hashes with no version chain, so `read_as_of` past a
> 1-hour live window refuses with `NotSupported` → HTTP 501 rather than
> answering with today's data
> (`crates/lunaris-storage-moon/src/as_of.rs`, pinned by
> `crates/lunaris-conformance/tests/run_as_of_moon_gap.rs`).

- An ingest **inserts** a row with `sys = (now, None)` and the supplied
  `valid` range.
- A correction (verifier supersede) **closes** the old row's `sys` at the
  correction time and inserts a new row — both stay on disk.
- A [forget](../guides/forget.md) (soft, the default) **closes** `sys` on the
  target rows; the audit log records the close. A time-travel query with
  `as_of` *before* the forget timestamp still sees the row.

So "what did the agent know at time T" is a storage query (`read_as_of` /
`.as_of(ts)` on the retrieval DSL, native bi-temporal on Moon), not a log
replay. Crash recovery therefore restores not
just the current state but the entire history, because the history *is* the
on-disk state.

## 3. Moon persistence model (for operators)

Moon combines **AOF (append-only file)** with **base RDB snapshots**:

1. Every write is appended to
   `<dir>/appendonlydir/moon.aof.<N>.incr.aof`. With `--appendfsync always`
   the append is `fsync`'d on every command — no data loss on `kill -9`
   after a successful `await ingest()`.
2. `BGREWRITEAOF` (or the auto-save rules in `--save "<seconds> <changes>"`)
   rotates the AOF: it writes a **base RDB** snapshot at
   `<dir>/appendonlydir/moon.aof.<N+1>.base.rdb`, then starts a new empty
   incremental at `moon.aof.<N+1>.incr.aof`.
3. On restart, Moon loads the newest `base.rdb` into memory, then replays the
   matching `incr.aof` on top. FT indices are rebuilt from the replayed
   HSETs automatically.

### 3.1 Launching Moon for durability

```bash
../moon/target/release/moon \
  --bind 127.0.0.1 --port 6380 \
  --dir /var/lib/moon \                    # where AOF + RDB live
  --shards 1 \
  --appendonly yes \                       # enable AOF
  --appendfsync always \                   # fsync every write (default: everysec)
  --save "3600 1 300 100"                  # RDB auto-rewrite rules
```

The `--save` rules follow the Redis convention: `seconds changes` pairs —
here, rewrite if either 1 change happened in 3600 s OR 100 changes in 300 s.
Pair them with an explicit `BGREWRITEAOF` before planned shutdowns if you
want a clean snapshot.

### 3.2 The base-RDB trap

**Without an existing base RDB, AOF-only state is unreplayable.** If you start
Moon fresh, ingest data, and `kill -9` before the first auto-save /
`BGREWRITEAOF` has run, the restart fails with:

```
Error: multi-part AOF replay failed
  AOF base RDB missing at moon.aof.1.base.rdb but incr moon.aof.1.incr.aof
  is 821715 bytes; refusing to replay incr against empty state
```

This is a deliberate Moon invariant — the AOF chain needs an anchor
snapshot, it won't silently replay against empty state.

**Safe patterns:**

1. Let the auto-save rules run naturally — `--save "60 1"` forces a base RDB
   within 60 s of any write, small enough for dev boxes.
2. Run `BGREWRITEAOF` explicitly after large bulk ingests; poll for the new
   `moon.aof.<N>.base.rdb` to appear on disk before you consider the data
   "durable".
3. Before a planned restart, run `BGREWRITEAOF` + wait for the file.

```bash
# Force a base RDB and wait for it before trusting durability
redis-cli -p 6380 BGREWRITEAOF
while ! ls /var/lib/moon/appendonlydir/*.base.rdb >/dev/null 2>&1; do
  sleep 0.2
done
```

> **Do not use `BGSAVE` for this.** `BGSAVE` writes `<dir>/dump.rdb`, a
> separate artefact that does NOT participate in AOF replay. Only
> `BGREWRITEAOF` produces the `appendonlydir/moon.aof.<N>.base.rdb` that
> Moon's recovery chain needs.

### 3.3 FT index lag (measurement gotcha, not a durability issue)

`await kb.ingest(...)` returns after the atomic envelope is ACKed. But Moon's
FT index `num_docs` can lag the underlying HSETs by a few hundred
milliseconds while the index materialises. If you snapshot `FT.INFO chunks`
immediately after the last ingest and then crash, post-recovery `num_docs`
can appear *higher* than the pre-crash snapshot — because the index caught up
during the interim.

This is not data loss. The underlying HSETs are already fsync'd. The
workaround in tests is to `sleep` 1–2 s before taking the "pre-crash"
snapshot. In production you don't need to do anything — the data is durable
either way.

### 3.4 WAL v3 + upgrade safety (Moon v0.7.0/v0.7.1)

The Moon v0.7.1 bump (2026-07-15) hardens the per-shard WAL to **WAL v3**:
a WAL record now commits durably as a whole or is discarded on replay
(atomic durable writes), and the FT term dictionary is persisted with the
WAL instead of rebuilt best-effort after a crash. The upgrade-safety fix
(#69, `segment_plane_scan`) matters most: v0.6.0 wrote MQ and temporal
plane records in a nested framing that v0.7's plane scan initially skipped —
without the fix, a v0.6→v0.7 restart would silently drop MQ backlog + PEL
state and temporal history. Lunaris pins this with a dedicated harness leg:

```bash
python scripts/test-recovery.py --upgrade-replay \
  --old-bin ~/.lunaris/bin/moon \
  --new-bin vendor/moon/target/release/moon
```

The v0.7.1 patch also fixes the SQ8 code-size mis-dispatch CPU error-storm
(#73 — relevant if you run Lunaris's opt-in SQ8 quantization) and makes
replica TTL expiry deterministic (#71 — relative expiries are rewritten to
absolute deadlines before entering the durable log, so an AOF replay
reproduces the master's expiry instant instead of restarting the countdown).

### 3.5 One Storage Kernel GA (Moon v0.8.0)

The v0.8.0 bump (2026-07-16, pinned at the post-release main commit
`e41aa671`) graduates this to **kill-9-lossless on every plane**, with
upstream's own scheduled crash-matrix CI (#352) now covering KV, vector,
graph, MQ, and temporal recovery. Two fixes matter operationally:
GraphUnion auto-merges rejected by the recall gate now **back off
exponentially** (#353) instead of livelocking the CPU and — through the
unflushed-segment stall guard — pausing writes after a restart; and the
pinned commit carries the DashTable split-retry fix (Moon PR #351) for a
deterministic recovery panic on hash-skewed checkpoint loads (the reason
the pin is a main SHA rather than the bare 0.8.0 tag). Disk-offload also
hardened: truthful `used_memory` under offload (#349) and batched spill
segments (#350).

## 4. Recovery procedure

### 4.1 Moon process crashed (kill-9, OOM, power loss)

```bash
# 1. Confirm Moon is really dead on the port
lsof -nP -iTCP:6380 -sTCP:LISTEN   # expect nothing

# 2. Inspect the AOF state
ls /var/lib/moon/appendonlydir/
# Healthy: one or more moon.aof.<N>.base.rdb files + matching .incr.aof
# Dangerous: only .incr.aof files, no .base.rdb → restart will refuse

# 3. Restart with the same --dir
../moon/target/release/moon --bind 127.0.0.1 --port 6380 \
  --dir /var/lib/moon --shards 1 --appendonly yes --appendfsync always \
  --save "60 1" &

# 4. Wait for ready
while ! redis-cli -p 6380 PING | grep -q PONG; do sleep 0.1; done

# 5. From the Lunaris side, just reconnect
python -c "import asyncio, lunaris; asyncio.run(lunaris.open('moon://127.0.0.1:6380'))"
```

Expected replay cost: ~20 ms per 1 MB of AOF on darwin-arm64. A 50-doc /
~820 KB AOF replays in 0.90 s end-to-end (including FT rebuild).

### 4.2 Lunaris client process crashed

Nothing to recover on the client side — the process was stateless. Any
ingest that hadn't `await`-ed its result was never committed. Just re-run.

### 4.3 Neither side's AOF has a base RDB

If `ls /var/lib/moon/appendonlydir/` shows only `.incr.aof` files and Moon
refuses to start, you have three options:

1. **Recoverable via synthetic replay.** Write a tiny script that starts a
   fresh Moon on a scratch dir, re-plays the `.incr.aof` entries via a custom
   RESP client, then runs `BGREWRITEAOF`. (Requires reading the `.incr.aof`
   format — see `moon/src/persistence/aof/` in the Moon repo.)
2. **Treat as data loss.** Wipe the corrupt data dir, start fresh, re-ingest
   from source.
3. **File a bug on Moon.** Ideally Moon would offer a
   `--allow-unrooted-aof-replay` flag for the base-less case.

Preventing this is easier than recovering: always run `BGREWRITEAOF` after
your first batch of writes when seeding a new deploy.

## 5. Testing recovery

A ready-to-run harness ships at `scripts/test-recovery.py`. It covers three
failure modes:

| test | what it verifies |
|---|---|
| `test_moon_kill` | SIGKILL Moon, restart, recover. Asserts dbsize, FT index list, `chunks_num_docs`, and top-10 text identity for 5 probe queries all match pre-crash. |
| `test_lunaris_kill` | SIGKILL a child python ingest at the halfway mark. Asserts Moon state is self-consistent (no torn writes), FT.SEARCH still runs. |
| `test_write_after_restart` | After `test_moon_kill`, writes a fresh doc with a unique anchor phrase and verifies it roundtrips through search. |

```bash
cd crates/lunaris-py
LUNARIS_TEST_MOON_URL="moon://127.0.0.1:6380" \
  uv run --with datasets --with python-ulid --with redis \
    python -u ../../scripts/test-recovery.py --docs 50
```

Reference numbers from the 2026-04-23 run (50 docs, darwin-arm64, release
Moon):

- Moon restart + AOF replay: 0.90 s wall
- Probe-identity check: 5/5 top-10 lists byte-identical
- Post-restart write roundtrip: PASS

Evidence log: `milestones/v0.1.1-bench/recovery-test.log`.

## 6. Known limitations

- **Single-shard only in this tested config.** Multi-shard Moon recovery
  semantics (cross-shard coordinator, manifest-level replay) aren't exercised
  by the harness yet.
- **AOF grows unboundedly without auto-rewrite.** Use `--save` rules or
  schedule `BGREWRITEAOF` — otherwise replay time grows linearly with write
  volume.
- **Pipeline workers replay idempotently** (`consolidator`, `verifier`) —
  they read from Moon Streams on start-up and resume from the last committed
  offset. No action needed on the Lunaris side.

## 7. Related references

- `.planning/architect/blueprint.md` §5.4 — durability contract
- `crates/lunaris-storage-moon/src/atomic.rs` — the `WriteOp` envelope shape
- `moon/src/persistence/` (upstream Moon repo) — AOF + RDB implementation
- [Ingesting Observations](../guides/ingest.md) / [The Retrieval DSL](../guides/retrieval-dsl.md) — the end-user API the recovery guarantees apply to
- [The Storage Backend](./backends.md) — Moon setup and its honest limits

If something here doesn't match the source, the source wins.

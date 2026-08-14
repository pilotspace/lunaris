#!/usr/bin/env bash
#
# Lunaris backup / restore-to-a-new-host drill (0.6.2).
#
# Companion doc: docs/operations/backup-restore.md — every number in that
# runbook's RPO/RTO table comes out of this script; re-run it to re-measure.
#
# WHAT THIS PROVES
#   A Lunaris workload written to a Moon-backed store can be backed up, moved
#   to a *different* instance (fresh data directory, different port, different
#   server process), and served back with **count and content equivalence** —
#   not merely a healthy PING. It also demonstrates, with automated
#   assertions, exactly what the two plausible naive procedures lose.
#
# LEGS
#   1  GREEN  cold backup  — quiesce (BGREWRITEAOF -> clean SHUTDOWN) -> copy
#                            -> restore to a fresh dir -> full equivalence.
#                            Also restores the PRE-rewrite copy to measure what
#                            BGREWRITEAOF is actually buying (RTO, not RPO).
#   2  RED    hot copy under concurrent write load, no quiesce -> the restored
#             instance STARTS CLEAN but is SILENTLY INCOMPLETE.
#   3  RED    BGSAVE/dump.rdb-style backup -> total, silent data loss.
#
# The RED legs are assertions in the same sense as the GREEN ones: they fail
# the script if the loss does NOT happen, because that would mean the runbook's
# warnings have gone stale against a newer Moon and must be re-derived.
#
# SAFETY
#   Runs only on ports 6395/6396 and only in a scratch directory it created.
#   Refuses to start if asked to use a well-known Lunaris port (6379/6380/6381
#   dev+live stores, 6399 dedicated bench Moon). Only ever stops Moon processes
#   whose PID it owns. Never issues FLUSHALL, anywhere.
#
# USAGE
#   scripts/backup-restore-drill.sh [--docs N] [--shards N] [--only 1|2|3] [--keep]
#
# ENV
#   MOON_BIN       moon binary            (default vendor/moon/target/release/moon)
#   WORKLOAD_BIN   workload driver        (default target/release/examples/... then debug)
#   DRILL_ROOT     scratch dir            (default $TMPDIR/lunaris-backup-drill.<pid>)
#   DRILL_DOCS     documents per corpus   (default 200)
#   DRILL_SHARDS   moon --shards            (default 1)
#   PORT_SRC/PORT_DST                     (default 6395 / 6396)
#
set -euo pipefail

# ── configuration ───────────────────────────────────────────────────────────
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MOON_BIN="${MOON_BIN:-$REPO_ROOT/vendor/moon/target/release/moon}"
DRILL_DOCS="${DRILL_DOCS:-200}"
PORT_SRC="${PORT_SRC:-6395}"
PORT_DST="${PORT_DST:-6396}"
DRILL_ROOT="${DRILL_ROOT:-${TMPDIR:-/tmp}/lunaris-backup-drill.$$}"
# Shard count for every Moon the drill starts. Backup/restore semantics are
# per-shard on disk (each shard keeps its own checkpoint under shard-N/), so a
# run at >1 is a materially different assertion, not a repeat.
DRILL_SHARDS="${DRILL_SHARDS:-1}"
KEEP=0
ONLY=""

# Ports that carry real data on developer machines and CI runners. The drill
# must never bind, write to, or shut down any of them.
FORBIDDEN_PORTS="6379 6380 6381 6399"

while [ $# -gt 0 ]; do
  case "$1" in
    --docs) DRILL_DOCS="$2"; shift 2 ;;
    --shards) DRILL_SHARDS="$2"; shift 2 ;;
    --only) ONLY="$2"; shift 2 ;;
    --keep) KEEP=1; shift ;;
    -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

if [ -z "${WORKLOAD_BIN:-}" ]; then
  if [ -x "$REPO_ROOT/target/release/examples/backup_restore_workload" ]; then
    WORKLOAD_BIN="$REPO_ROOT/target/release/examples/backup_restore_workload"
  else
    WORKLOAD_BIN="$REPO_ROOT/target/debug/examples/backup_restore_workload"
  fi
fi

# ── plumbing ────────────────────────────────────────────────────────────────
FAILURES=0
OWNED_PIDS=""

now()      { perl -MTime::HiRes -e 'printf "%.3f", Time::HiRes::time()'; }
elapsed()  { perl -e 'printf "%.3f", $ARGV[1]-$ARGV[0]' "$1" "$(now)"; }
log()      { printf '%s\n' "$*"; }
hdr()      { printf '\n════════ %s ════════\n' "$*"; }
die()      { printf 'FATAL: %s\n' "$*" >&2; exit 1; }

pass() { printf '  [PASS] %s\n' "$*"; }
fail() { printf '  [FAIL] %s\n' "$*"; FAILURES=$((FAILURES + 1)); }

# assert_eq <label> <expected> <actual>
assert_eq() {
  if [ "$2" = "$3" ]; then pass "$1: $3"; else fail "$1: expected [$2], got [$3]"; fi
}

# assert_lt <label> <actual> <bound>  — actual must be strictly less than bound
assert_lt() {
  if [ "$2" -lt "$3" ] 2>/dev/null; then pass "$1: $2 < $3"; else fail "$1: expected $2 < $3"; fi
}

cleanup() {
  local rc=$?
  # `wait` inside a redirected group so the shell's "Killed: 9" job-reaping
  # notice does not masquerade as drill output.
  for p in $OWNED_PIDS; do
    kill -9 "$p" >/dev/null 2>&1 || true
    { wait "$p"; } >/dev/null 2>&1 || true
  done
  if [ "$KEEP" -eq 1 ]; then
    log ""
    log "--keep: scratch retained at $DRILL_ROOT"
  else
    rm -rf "$DRILL_ROOT"
  fi
  exit $rc
}
trap cleanup EXIT INT TERM

# ── moon lifecycle (only ever touches PIDs we spawned) ──────────────────────
port_is_free() {
  ! lsof -nP -iTCP:"$1" -sTCP:LISTEN >/dev/null 2>&1
}

# start_moon <port> <datadir> <logfile> [extra flags...] — sets $MOON_PID
#
# Deliberately NOT a command-substitution helper: the PID must be recorded in
# the parent shell's $OWNED_PIDS, and a `$(...)` subshell would discard it,
# leaving the cleanup trap unable to reap the process it spawned.
#
# Production-shaped durability flags: AOF on, fsync every write, and a
# CONSERVATIVE --save so no background rewrite silently rescues a leg that is
# meant to demonstrate an un-anchored directory. --disk-free-min-pct 1 keeps
# the diskfull guard from pausing writes on the >90%-full dev boxes this
# project runs live-Moon work on (see docs/durability.md).
MOON_PID=""
start_moon() {
  local port="$1" dir="$2" logf="$3"; shift 3
  mkdir -p "$dir"
  "$MOON_BIN" --bind 127.0.0.1 --port "$port" --dir "$dir" \
    --shards "$DRILL_SHARDS" --protected-mode no \
    --appendonly yes --appendfsync always --save "3600 1" \
    --disk-free-min-pct 1 "$@" >"$logf" 2>&1 &
  MOON_PID=$!
  OWNED_PIDS="$OWNED_PIDS $MOON_PID"
}

# wait_ping <port> <timeout_s> — returns 0 and prints the wall seconds to ready
wait_ping() {
  local port="$1" budget="${2:-30}" t0
  t0="$(now)"
  while :; do
    if redis-cli -p "$port" PING 2>/dev/null | grep -q PONG; then
      elapsed "$t0"; return 0
    fi
    if [ "$(perl -e 'print(($ARGV[1]-$ARGV[0]) > $ARGV[2] ? 1 : 0)' "$t0" "$(now)" "$budget")" = "1" ]; then
      return 1
    fi
    sleep 0.1
  done
}

owned() { case " $OWNED_PIDS " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# stop_moon_clean <pid> <port> — the runbook's quiesce step. Prints wall secs.
stop_moon_clean() {
  local pid="$1" port="$2" t0
  owned "$pid" || die "refusing to stop pid $pid — not spawned by this drill"
  t0="$(now)"
  redis-cli -p "$port" SHUTDOWN >/dev/null 2>&1 || true
  local i=0
  while kill -0 "$pid" 2>/dev/null; do
    i=$((i + 1)); [ "$i" -gt 300 ] && { kill -9 "$pid" 2>/dev/null || true; break; }
    sleep 0.1
  done
  wait "$pid" 2>/dev/null || true
  elapsed "$t0"
}

# kill_moon <pid> — SIGKILL, for legs that model an unplanned outage
kill_moon() {
  owned "$1" || die "refusing to kill pid $1 — not spawned by this drill"
  kill -9 "$1" 2>/dev/null || true
  wait "$1" 2>/dev/null || true
  sleep 0.3
}

# ── probes ──────────────────────────────────────────────────────────────────
dbsize()  { redis-cli -p "$1" DBSIZE 2>/dev/null | tr -d '\r' | head -1; }
# Moon's persistent instance identity. NOT `run_id` (absent from this build's
# INFO) — `master_replid`, which Moon stores in <dir>/replication.state and
# therefore carries into any verbatim restore. `head -1` because Moon 0.8.5
# emits the `# Replication` section twice in one INFO reply.
repl_id() {
  redis-cli -p "$1" INFO replication 2>/dev/null | tr -d '\r' \
    | sed -n 's/^master_replid://p' | head -1
}
moon_ver(){ redis-cli -p "$1" INFO server 2>/dev/null | tr -d '\r' | sed -n 's/^moon_version://p'; }

# ft_num_docs <port> <scope> — documents in this scope's chunk index
ft_num_docs() {
  redis-cli -p "$1" FT.INFO "lunaris_$2_chunks_idx" 2>/dev/null \
    | tr -d '\r' | grep -A1 '^num_docs$' | tail -1
}

aof_incr_bytes() {
  # newest incremental AOF, in bytes (0 when the dir has none)
  local n
  n="$(ls -l "$1"/appendonlydir/*.incr.aof 2>/dev/null | awk '{s+=$5} END {print s+0}')"
  echo "${n:-0}"
}
aof_base_bytes() {
  local n
  n="$(ls -l "$1"/appendonlydir/*.base.rdb 2>/dev/null | awk '{s+=$5} END {print s+0}')"
  echo "${n:-0}"
}
dir_bytes() { du -sk "$1" 2>/dev/null | awk '{print $1*1024}'; }

# bgrewriteaof_and_wait <port> <datadir> <timeout_s> — prints wall secs.
#
# Polls for BOTH signals, because either alone lies: `aof_rewrite_in_progress`
# can read 0 in the gap before the child is forked, and a `.base.rdb` appears
# on disk before it is fully written. Requires a base whose byte count GREW
# over the pre-rewrite state (a fresh dir already ships a 10-byte empty-state
# anchor on Moon >= 0.8.5 — see the runbook's "base-RDB trap" note).
bgrewriteaof_and_wait() {
  local port="$1" dir="$2" budget="${3:-60}" t0 before
  before="$(aof_base_bytes "$dir")"
  t0="$(now)"
  redis-cli -p "$port" BGREWRITEAOF >/dev/null 2>&1 || true
  while :; do
    local inprog after
    inprog="$(redis-cli -p "$port" INFO persistence 2>/dev/null | tr -d '\r' \
              | sed -n 's/^aof_rewrite_in_progress://p')"
    after="$(aof_base_bytes "$dir")"
    if [ "${inprog:-0}" = "0" ] && [ "${after:-0}" -gt "${before:-0}" ]; then
      elapsed "$t0"; return 0
    fi
    if [ "$(perl -e 'print(($ARGV[1]-$ARGV[0]) > $ARGV[2] ? 1 : 0)' "$t0" "$(now)" "$budget")" = "1" ]; then
      elapsed "$t0"; return 1
    fi
    sleep 0.1
  done
}

# wl_write <port> <scope> <docs> <out> — ingest through the real Lunaris path.
#
# Fails the drill loudly: a workload that cannot be written is not a durability
# result. The most likely cause is DRILL_SHARDS > 1 — Lunaris' single-envelope
# `atomic_write` (INGEST-04) is a Moon TXN, and Moon rejects cross-shard TXNs
# ("TXN does not support cross-shard writes"), so Lunaris does not run against
# a sharded Moon at all today. See the runbook's multi-shard note.
wl_write() {
  if ! "$WORKLOAD_BIN" write --url "moon://127.0.0.1:$1" --scope "$2" --docs "$3" --out "$4" >/dev/null; then
    if [ "$DRILL_SHARDS" != "1" ]; then
      die "ingest failed at --shards $DRILL_SHARDS.
  Lunaris writes one cross-key TXN per episode and Moon rejects cross-shard
  TXNs, so a sharded Moon is not a supported Lunaris backend today. Re-run the
  drill with --shards 1; multi-shard backup/restore is moot until that lands."
    fi
    die "workload ingest failed against port $1 scope $2"
  fi
}
wl_verify() {
  # verify never fails the script on its own; the JSON comparison judges.
  "$WORKLOAD_BIN" verify --url "moon://127.0.0.1:$1" --scope "$2" --docs "$3" --out "$4" \
    ${5:+--expect-hits "$5"} ${6:+--settle-timeout-secs "$6"} >/dev/null 2>&1 || true
}

json_get() { python3 -c 'import json,sys;d=json.load(open(sys.argv[1]));
p=sys.argv[2].split(".")
for k in p: d=d.get(k,{}) if isinstance(d,dict) else {}
print(d if not isinstance(d,(dict,list)) else len(d))' "$1" "$2" 2>/dev/null || echo 0; }

# fingerprint_equal <before.json> <after.json> — content equivalence, ordered
# fields excluded. Prints a human diff summary and returns non-zero on drift.
fingerprint_equal() {
  python3 - "$1" "$2" <<'PY'
import json, sys
a = json.load(open(sys.argv[1]))
b = json.load(open(sys.argv[2]))
if "connect_failed" in b:
    print("    restored instance refused the connection: %s" % b["connect_failed"])
    sys.exit(1)
ok = True
ta, tb = a["corpus"]["texts"], b["corpus"]["texts"]
if len(ta) != len(tb):
    print("    corpus hit_count %d -> %d" % (len(ta), len(tb))); ok = False
missing = [t for t in ta if t not in tb]
extra   = [t for t in tb if t not in ta]
if missing:
    print("    %d corpus text(s) MISSING after restore, first: %s" % (len(missing), missing[0][:80]))
    ok = False
if extra:
    print("    %d unexpected corpus text(s) after restore, first: %s" % (len(extra), extra[0][:80]))
    ok = False
fa, fb = a["per_doc"]["found"], b["per_doc"]["found"]
if fa != fb:
    print("    per-doc found %d -> %d (missing: %s)"
          % (fa, fb, ",".join(b["per_doc"]["missing"][:5]))); ok = False
sys.exit(0 if ok else 1)
PY
}

# ── preflight ───────────────────────────────────────────────────────────────
hdr "preflight"
for p in $PORT_SRC $PORT_DST; do
  for bad in $FORBIDDEN_PORTS; do
    [ "$p" = "$bad" ] && die "port $p is a reserved Lunaris store port — refusing to run"
  done
  port_is_free "$p" || die "port $p is already in use; the drill will not share a port"
done
[ -x "$MOON_BIN" ]     || die "moon binary not found/executable at $MOON_BIN
  build it with: cargo build --release --bin moon   (in vendor/moon)"
[ -x "$WORKLOAD_BIN" ] || die "workload driver not found at $WORKLOAD_BIN
  build it with: cargo build -p lunaris-memory --no-default-features \\
    --example backup_restore_workload --release"
case "$DRILL_ROOT" in
  /|/Users|/Volumes|"$REPO_ROOT") die "unsafe DRILL_ROOT: $DRILL_ROOT" ;;
esac
mkdir -p "$DRILL_ROOT"
log "  moon        : $MOON_BIN"
log "  workload    : $WORKLOAD_BIN"
log "  scratch     : $DRILL_ROOT"
log "  docs/corpus : $DRILL_DOCS"
log "  shards      : $DRILL_SHARDS"
log "  ports       : src=$PORT_SRC dst=$PORT_DST"

RESULTS="$DRILL_ROOT/results.txt"
: >"$RESULTS"
record() { printf '%s=%s\n' "$1" "$2" >>"$RESULTS"; log "  · $1 = $2"; }

run_leg() { [ -z "$ONLY" ] || [ "$ONLY" = "$1" ]; }

# ════════════════════════════════════════════════════════════════════════════
# LEG 1 — GREEN: cold backup, restore onto a fresh instance
# ════════════════════════════════════════════════════════════════════════════
if run_leg 1; then
hdr "LEG 1 (GREEN) — quiesced backup, restore to a NEW instance"
L1="$DRILL_ROOT/leg1"; mkdir -p "$L1"
SCOPE1="drillmain"

start_moon "$PORT_SRC" "$L1/primary" "$L1/primary.log"; SRC_PID="$MOON_PID"
T_UP="$(wait_ping "$PORT_SRC" 30)" || die "source Moon did not come up (see $L1/primary.log)"
SRC_RUN_ID="$(repl_id "$PORT_SRC")"
record "moon_version" "$(moon_ver "$PORT_SRC")"
record "shards" "$DRILL_SHARDS"
record "source_startup_secs" "$T_UP"

log "  ingesting $DRILL_DOCS documents through the Lunaris ingest path..."
T0="$(now)"
wl_write "$PORT_SRC" "$SCOPE1" "$DRILL_DOCS" "$L1/before.json"
record "ingest_secs" "$(elapsed "$T0")"
SRC_DBSIZE="$(dbsize "$PORT_SRC")"
SRC_FTDOCS="$(ft_num_docs "$PORT_SRC" "$SCOPE1")"
record "source_dbsize" "$SRC_DBSIZE"
record "source_ft_num_docs" "$SRC_FTDOCS"
record "aof_incr_bytes_pre_rewrite" "$(aof_incr_bytes "$L1/primary")"
record "aof_base_bytes_pre_rewrite" "$(aof_base_bytes "$L1/primary")"

# B0: the UN-anchored copy, kept only to measure what BGREWRITEAOF buys.
cp -R "$L1/primary" "$L1/backup-prerewrite"

log "  BGREWRITEAOF (anchor the AOF chain, bound replay time)..."
T_REWRITE="$(bgrewriteaof_and_wait "$PORT_SRC" "$L1/primary" 120)" \
  || fail "BGREWRITEAOF did not complete within budget (took ${T_REWRITE}s)"
record "bgrewriteaof_secs" "$T_REWRITE"
record "aof_incr_bytes_post_rewrite" "$(aof_incr_bytes "$L1/primary")"
record "aof_base_bytes_post_rewrite" "$(aof_base_bytes "$L1/primary")"

log "  clean SHUTDOWN..."
T_STOP="$(stop_moon_clean "$SRC_PID" "$PORT_SRC")"
record "clean_shutdown_secs" "$T_STOP"
port_is_free "$PORT_SRC" && pass "source port released" || fail "source port still bound"

log "  taking the backup (cp -R of the quiesced data dir)..."
T0="$(now)"
cp -R "$L1/primary" "$L1/backup"
record "backup_copy_secs" "$(elapsed "$T0")"
record "backup_bytes" "$(dir_bytes "$L1/backup")"

log "  RESTORE: copying the backup to a fresh path (models a new host)..."
T_RESTORE_START="$(now)"
cp -R "$L1/backup" "$L1/restored"
T_COPY="$(elapsed "$T_RESTORE_START")"
record "restore_copy_secs" "$T_COPY"

start_moon "$PORT_DST" "$L1/restored" "$L1/restored.log"; DST_PID="$MOON_PID"
T_READY="$(wait_ping "$PORT_DST" 60)" || die "restored Moon did not come up (see $L1/restored.log)"
record "restore_start_to_ping_secs" "$T_READY"
DST_RUN_ID="$(repl_id "$PORT_DST")"

log "  verifying the restored instance..."
wl_verify "$PORT_DST" "$SCOPE1" "$DRILL_DOCS" "$L1/after.json" "$DRILL_DOCS" 60
T_RTO="$(elapsed "$T_RESTORE_START")"
record "restore_settle_secs" "$(json_get "$L1/after.json" settle_secs)"
record "RTO_total_secs" "$T_RTO"

log ""
log "  ── assertions ──"
if [ "$DST_PID" != "$SRC_PID" ] && ! kill -0 "$SRC_PID" 2>/dev/null; then
  pass "restored instance is a NEW server process (pid $SRC_PID dead -> $DST_PID live, port $PORT_SRC -> $PORT_DST, dir primary/ -> restored/)"
else
  fail "restore is not running as an independent process — the drill proved nothing"
fi
# Moon persists its replication identity in <dir>/replication.state, so a
# verbatim restore inherits the SOURCE's run_id. Pinned as an assertion, not a
# warning: if a future Moon regenerates the id on load, the runbook's
# "scrub replication.state when cloning" step becomes unnecessary and must be
# revisited rather than silently kept.
assert_eq "replication identity carried into a verbatim restore" "$SRC_RUN_ID" "$DST_RUN_ID"
assert_eq "DBSIZE equivalence"       "$SRC_DBSIZE" "$(dbsize "$PORT_DST")"
assert_eq "FT chunk num_docs"        "$SRC_FTDOCS" "$(ft_num_docs "$PORT_DST" "$SCOPE1")"
assert_eq "per-doc recall count"     "$DRILL_DOCS" "$(json_get "$L1/after.json" per_doc.found)"
if fingerprint_equal "$L1/before.json" "$L1/after.json"; then
  pass "content equivalence: every chunk text recalled verbatim after restore"
else
  fail "content equivalence: restored corpus differs from the pre-backup corpus"
fi
record "RPO_docs_lost" "$(( DRILL_DOCS - $(json_get "$L1/after.json" per_doc.found) ))"

log ""
log "  ── clone-safe restore: same backup, replication.state scrubbed ──"
stop_moon_clean "$DST_PID" "$PORT_DST" >/dev/null
cp -R "$L1/backup" "$L1/restored-clone"
rm -f "$L1/restored-clone/replication.state"
start_moon "$PORT_DST" "$L1/restored-clone" "$L1/restored-clone.log"; DST_PID="$MOON_PID"
if wait_ping "$PORT_DST" 60 >/dev/null; then
  CLONE_RUN_ID="$(repl_id "$PORT_DST")"
  if [ -n "$CLONE_RUN_ID" ] && [ "$CLONE_RUN_ID" != "$SRC_RUN_ID" ]; then
    pass "scrubbing replication.state yields a FRESH replication id ($CLONE_RUN_ID)"
  else
    fail "replication id survived the scrub — cloning a live master is unsafe"
  fi
  wl_verify "$PORT_DST" "$SCOPE1" "$DRILL_DOCS" "$L1/after-clone.json" "$DRILL_DOCS" 60
  if fingerprint_equal "$L1/before.json" "$L1/after-clone.json"; then
    pass "scrubbing replication.state costs no data"
  else
    fail "scrubbing replication.state lost data — do NOT recommend it"
  fi
else
  fail "restore with replication.state removed did not start"
fi
stop_moon_clean "$DST_PID" "$PORT_DST" >/dev/null || true

log ""
log "  ── control: restore the PRE-BGREWRITEAOF copy (RTO comparison) ──"
cp -R "$L1/backup-prerewrite" "$L1/restored-prerewrite"
start_moon "$PORT_DST" "$L1/restored-prerewrite" "$L1/restored-prerewrite.log"; DST_PID="$MOON_PID"
if T_READY2="$(wait_ping "$PORT_DST" 60)"; then
  record "prerewrite_start_to_ping_secs" "$T_READY2"
  wl_verify "$PORT_DST" "$SCOPE1" "$DRILL_DOCS" "$L1/after-prerewrite.json" "$DRILL_DOCS" 60
  if fingerprint_equal "$L1/before.json" "$L1/after-prerewrite.json"; then
    pass "un-anchored copy ALSO restores intact on this Moon (base-RDB trap not reachable)"
  else
    pass "un-anchored copy loses data — BGREWRITEAOF is load-bearing for RPO here"
  fi
else
  pass "un-anchored copy is UNLOADABLE — BGREWRITEAOF is mandatory (see $L1/restored-prerewrite.log)"
  record "prerewrite_start_to_ping_secs" "REFUSED"
fi
stop_moon_clean "$DST_PID" "$PORT_DST" >/dev/null || true
fi

# ════════════════════════════════════════════════════════════════════════════
# LEG 2 — RED: hot copy under write load produces a silently partial backup
# ════════════════════════════════════════════════════════════════════════════
if run_leg 2; then
hdr "LEG 2 (RED) — hot cp -R under concurrent writes: silent partial backup"
L2="$DRILL_ROOT/leg2"; mkdir -p "$L2"
SCOPE_BASE="drillbase"; SCOPE_HOT="drillhot"
HOT_DOCS=$(( DRILL_DOCS * 2 ))

start_moon "$PORT_SRC" "$L2/primary" "$L2/primary.log"; SRC_PID="$MOON_PID"
wait_ping "$PORT_SRC" 30 >/dev/null || die "leg2 source Moon did not come up"
wl_write "$PORT_SRC" "$SCOPE_BASE" "$DRILL_DOCS" "$L2/base-before.json"
bgrewriteaof_and_wait "$PORT_SRC" "$L2/primary" 120 >/dev/null || true
log "  baseline of $DRILL_DOCS docs is anchored; starting a $HOT_DOCS-doc writer"

"$WORKLOAD_BIN" write --url "moon://127.0.0.1:$PORT_SRC" --scope "$SCOPE_HOT" \
  --docs "$HOT_DOCS" --out "$L2/hot-before.json" >/dev/null 2>&1 &
HOT_PID=$!
sleep 0.4                                   # let acked writes accumulate
T0="$(now)"
cp -R "$L2/primary" "$L2/backup"            # ← the naive procedure
record "hot_copy_secs" "$(elapsed "$T0")"
wait "$HOT_PID" 2>/dev/null || true
HOT_ACKED="$(json_get "$L2/hot-before.json" per_doc.found)"
record "hot_docs_acked_on_source" "$HOT_ACKED"
kill_moon "$SRC_PID"

start_moon "$PORT_DST" "$L2/backup" "$L2/backup.log"; DST_PID="$MOON_PID"
log ""
log "  ── assertions ──"
if wait_ping "$PORT_DST" 60 >/dev/null; then
  pass "restored instance STARTS CLEAN — the corruption is not announced"
  wl_verify "$PORT_DST" "$SCOPE_BASE" "$DRILL_DOCS" "$L2/base-after.json"
  wl_verify "$PORT_DST" "$SCOPE_HOT"  "$HOT_DOCS"   "$L2/hot-after.json"
  HOT_RESTORED="$(json_get "$L2/hot-after.json" per_doc.found)"
  record "hot_docs_restored" "$HOT_RESTORED"
  record "hot_docs_lost" "$(( HOT_ACKED - HOT_RESTORED ))"
  if fingerprint_equal "$L2/base-before.json" "$L2/base-after.json"; then
    pass "pre-copy (anchored) corpus survived intact"
  else
    fail "pre-copy corpus was ALSO damaged — worse than documented; re-derive the runbook"
  fi
  assert_lt "acked-during-copy documents LOST (silently)" "$HOT_RESTORED" "$HOT_ACKED"
else
  # Also an acceptable RED outcome, but a different warning for the runbook.
  pass "restored instance REFUSES TO START (loud failure, not silent)"
  record "hot_docs_restored" "REFUSED"
fi
stop_moon_clean "$DST_PID" "$PORT_DST" >/dev/null || true
fi

# ════════════════════════════════════════════════════════════════════════════
# LEG 3 — RED: an RDB-snapshot ("BGSAVE + copy dump.rdb") backup loses everything
# ════════════════════════════════════════════════════════════════════════════
if run_leg 3; then
hdr "LEG 3 (RED) — BGSAVE/dump.rdb backup: total silent loss"
L3="$DRILL_ROOT/leg3"; mkdir -p "$L3"
SCOPE3="drillrdb"

start_moon "$PORT_SRC" "$L3/primary" "$L3/primary.log"; SRC_PID="$MOON_PID"
wait_ping "$PORT_SRC" 30 >/dev/null || die "leg3 source Moon did not come up"
wl_write "$PORT_SRC" "$SCOPE3" "$DRILL_DOCS" "$L3/before.json"
redis-cli -p "$PORT_SRC" BGSAVE >/dev/null 2>&1 || true
sleep 2
mkdir -p "$L3/backup"
DUMP_FOUND=0
if [ -f "$L3/primary/dump.rdb" ]; then
  cp "$L3/primary/dump.rdb" "$L3/backup/"
  DUMP_FOUND=1
fi
record "bgsave_produced_dump_rdb" "$DUMP_FOUND"
kill_moon "$SRC_PID"

start_moon "$PORT_DST" "$L3/backup" "$L3/backup.log"; DST_PID="$MOON_PID"
log ""
log "  ── assertions ──"
if wait_ping "$PORT_DST" 60 >/dev/null; then
  wl_verify "$PORT_DST" "$SCOPE3" "$DRILL_DOCS" "$L3/after.json"
  RDB_FOUND="$(json_get "$L3/after.json" per_doc.found)"
  record "rdb_backup_docs_restored" "$RDB_FOUND"
  pass "instance starts happily on the RDB-only 'backup' — no error is raised"
  assert_eq "documents recoverable from an RDB-only backup" "0" "$RDB_FOUND"
  assert_eq "DBSIZE of the RDB-only restore" "0" "$(dbsize "$PORT_DST")"
else
  fail "RDB-only restore did not start at all — expected a silent EMPTY instance"
fi
stop_moon_clean "$DST_PID" "$PORT_DST" >/dev/null || true
fi

# ── summary ─────────────────────────────────────────────────────────────────
hdr "MEASUREMENTS"
cat "$RESULTS"
hdr "SUMMARY"
if [ "$FAILURES" -eq 0 ]; then
  log "  ALL ASSERTIONS PASSED"
  exit 0
fi
log "  $FAILURES ASSERTION(S) FAILED"
exit 1

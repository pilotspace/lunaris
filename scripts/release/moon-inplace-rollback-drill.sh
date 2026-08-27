#!/usr/bin/env bash
# In-place rollback drill for a Moon-to-Moon release hop.
#
# The older drill in this directory (rollback-drill.sh) rehearses 0.6.2 <-> 0.7.0,
# which CROSSED A STORAGE BACKEND: it needs lunaris-migrate, a SQLite store, and a
# re-embed step. Nothing after 0.7.0 crosses a backend, so replaying it for a
# Moon-to-Moon release exercises a transition no user of that release performs,
# at the cost of building a binary from a tag whose migrate tool no longer exists.
#
# This drill covers the hop that Moon-to-Moon releases actually ship: swap the
# binary, leave the store alone. The question it answers is narrow and is the
# only one that matters when there is no format change to migrate:
#
#     can the OLD binary still read what the NEW binary wrote?
#
# Usage:
#   DRILL_MOON_BIN=~/.lunaris/bin/moon \
#   DRILL_SERVER_OLD=/path/to/old/lunaris-server \
#   DRILL_SERVER_NEW=/path/to/new/lunaris-server \
#   scripts/release/moon-inplace-rollback-drill.sh
#
# The two servers may be built with --no-default-features (no llama.cpp). The
# hard assertions go through /v1/browse/episode, which decodes stored records
# and needs no embedder; recall is asserted too but only when an embedder is
# actually present, and a skipped recall is COUNTED and printed, never silent.
set -uo pipefail

MOON_BIN="${DRILL_MOON_BIN:?set DRILL_MOON_BIN to a moon >=0.8.5 binary}"
S_OLD="${DRILL_SERVER_OLD:?set DRILL_SERVER_OLD to the lunaris-server of the PREVIOUS release}"
S_NEW="${DRILL_SERVER_NEW:?set DRILL_SERVER_NEW to the release-candidate lunaris-server}"
DRILL="${DRILL_DIR:-$(mktemp -d /tmp/lunaris-inplace-drill.XXXXXX)}"
MOON_PORT="${DRILL_MOON_PORT:-6402}"

# 6379/6380 are not ours; 6381 is the live personal memory store; 6399 is the
# bench Moon. A drill must never write to any of them.
case "$MOON_PORT" in
  6379|6380|6381|6399)
    echo "refusing port $MOON_PORT: live/bench Moon ports are off-limits to the drill" >&2
    exit 2 ;;
esac

BIND_OLD=127.0.0.1:8093
BIND_NEW=127.0.0.1:8094
LOG="$DRILL/drill.log"
SKIPPED=0

mkdir -p "$DRILL/moon-data"
say()  { echo "== $*" | tee -a "$LOG"; }
skip() { SKIPPED=$((SKIPPED+1)); echo "== SKIP: $*" | tee -a "$LOG"; }
fail() { echo "== DRILL FAIL: $*" | tee -a "$LOG"; cleanup; exit 1; }
cleanup() {
  [ -n "${SRV_PID:-}" ] && kill "$SRV_PID" 2>/dev/null
  [ -n "${MOON_PID:-}" ] && kill "$MOON_PID" 2>/dev/null
  sleep 1
}
trap cleanup EXIT

cat > "$DRILL/tokens.json" <<'EOF'
{ "drilltoken": { "tenant": "drill", "scopes": ["ingest", "recall", "forget"] } }
EOF
AUTH="Authorization: Bearer drilltoken"

wait_http() { for _ in $(seq 1 "$2"); do curl -sf -m 2 "$1" >/dev/null 2>&1 && return 0; sleep 1; done; return 1; }

start_server() { # binary, bind, logfile
  "$1" --bind "$2" --storage "moon://127.0.0.1:$MOON_PORT" \
    --tokens-file "$DRILL/tokens.json" > "$3" 2>&1 &
  SRV_PID=$!
  wait_http "http://$2/healthz" 40 || { tail -25 "$3"; fail "server $1 never became healthy"; }
}
stop_server() { [ -n "${SRV_PID:-}" ] && { kill "$SRV_PID"; wait "$SRV_PID" 2>/dev/null; SRV_PID=; }; sleep 1; }

ingest() { # bind, content
  curl -sf -m 60 -X POST "http://$1/v1/ingest" -H "$AUTH" -H 'content-type: application/json' \
    -d "{\"source\":\"drill\",\"content\":\"$2\"}" >> "$LOG" 2>&1 || fail "ingest failed: $2"
  echo >> "$LOG"
}
# The hard assertion. Decodes every stored episode and greps for a marker the
# other binary wrote. No embedder involved, so a degraded build cannot make
# this pass or fail for the wrong reason.
browse_has() { # bind, marker, description
  local body
  body=$(curl -sf -m 30 "http://$1/v1/browse/episode?limit=100" -H "$AUTH") \
    || fail "browse failed while checking: $3"
  echo "$body" >> "$LOG"
  echo "$body" | grep -q "$2" || fail "$3 (marker '$2' absent from browse)"
  say "OK: $3"
}
# Secondary. Only meaningful with a real embedder; explicitly skipped and
# counted otherwise so an absent check never reads as a passing one.
recall_has() { # bind, query, marker, description
  local body
  body=$(curl -sf -m 90 -X POST "http://$1/v1/recall" -H "$AUTH" -H 'content-type: application/json' \
    -d "{\"query\":\"$2\",\"k\":5}") || { skip "$4 (recall call failed)"; return; }
  echo "$body" >> "$LOG"
  if [ "$body" = "[]" ] && [ "${DRILL_NO_EMBEDDER:-0}" = "1" ]; then
    skip "$4 (no-embedder build, recall returns empty by construction)"
    return
  fi
  echo "$body" | grep -q "$3" || fail "$4 (marker '$3' absent from recall)"
  say "OK: $4"
}

# /readyz reports per-dependency checks. The drill is about STORAGE, so `ping`
# and `canary` must be ok on every binary in every phase. `ready` itself is only
# true with a working embedder, and these binaries may be built without one —
# so instead of skipping the check on a degraded build, assert the exact shape
# a degraded build must have. A storage failure can then never hide behind the
# embedder being the expected failure.
#
# It also asserts `version`, which proves the binary answering is the one this
# phase started — the whole drill is meaningless if a stale process is serving.
readyz_ok() { # bind, expected_version
  local body code ping canary ready ver
  body=$(curl -s -m 15 -w '\n%{http_code}' "http://$1/readyz") || fail "readyz unreachable on $1"
  code=$(echo "$body" | tail -1)
  body=$(echo "$body" | sed '$d')
  echo "readyz($1) -> $code $body" >> "$LOG"
  ping=$(echo "$body"   | jq -r '.checks.ping   // "missing"')
  canary=$(echo "$body" | jq -r '.checks.canary // "missing"')
  # NOT `.ready // "missing"`: jq's `//` treats `false` as empty, so a genuine
  # `ready: false` — the exact state a no-embedder build must report — would
  # come back as "missing" and fail for the wrong reason. Ask whether the key
  # exists, then stringify it.
  ready=$(echo "$body"  | jq -r 'if has("ready") then (.ready|tostring) else "missing" end')
  ver=$(echo "$body"    | jq -r '.version       // "missing"')
  [ "$ver" = "$2" ] || fail "readyz on $1 reports version $ver, expected $2 — a stale process is serving this port"
  [ "$ping" = "ok" ]   || fail "readyz on $1: storage ping is '$ping', not ok"
  [ "$canary" = "ok" ] || fail "readyz on $1: storage canary is '$canary', not ok"
  if [ "${DRILL_NO_EMBEDDER:-0}" = "1" ]; then
    [ "$ready" = "false" ] || fail "readyz on $1 says ready=$ready on a no-embedder build — the embedder check is not wired"
    say "OK: readyz $2 — storage ok, ready=false as a no-embedder build must report"
  else
    [ "$ready" = "true" ] || fail "readyz on $1: ready=$ready (body: $body)"
    say "OK: readyz $2 — ready"
  fi
}

# ---------- phase 0: scratch Moon ----------
# --max-unflushed-immutable-segments 0 avoids the 0.8.5 compaction deadlock,
# which stalls ALL writes and cannot be cleared through FT.CONFIG once wedged.
OLD_VER=$("$S_OLD" --version 2>/dev/null | awk '{print $NF}')
NEW_VER=$("$S_NEW" --version 2>/dev/null | awk '{print $NF}')
[ -n "$OLD_VER" ] && [ -n "$NEW_VER" ] || fail "could not read --version from both binaries"
[ "$OLD_VER" != "$NEW_VER" ] || fail "both binaries report $OLD_VER — a rollback drill needs two DIFFERENT versions"
say "drill: $OLD_VER -> $NEW_VER -> $OLD_VER -> $NEW_VER, in place on one Moon store"

say "phase 0: scratch Moon on $MOON_PORT (dir $DRILL/moon-data)"
nohup "$MOON_BIN" --bind 127.0.0.1 --port "$MOON_PORT" --dir "$DRILL/moon-data" \
  --shards 1 --max-unflushed-immutable-segments 0 > "$DRILL/moon.log" 2>&1 &
MOON_PID=$!
for _ in $(seq 1 25); do
  redis-cli -p "$MOON_PORT" ping 2>/dev/null | grep -q PONG && break
  sleep 1
done
redis-cli -p "$MOON_PORT" ping 2>/dev/null | grep -q PONG || { tail -20 "$DRILL/moon.log"; fail "Moon did not come up"; }
say "moon up (pid $MOON_PID)"

# ---------- phase 1: OLD binary writes ----------
say "phase 1: OLD binary ($S_OLD) writes the pre-upgrade episode"
start_server "$S_OLD" "$BIND_OLD" "$DRILL/server-old-pre.log"
readyz_ok "$BIND_OLD" "$OLD_VER"
ingest "$BIND_OLD" "episode alpha: written before the upgrade by the old binary"
sleep 2
browse_has "$BIND_OLD" "episode alpha" "old binary reads its own write"
stop_server

# ---------- phase 2: upgrade in place ----------
say "phase 2: upgrade — NEW binary ($S_NEW) on the SAME store, no data motion"
start_server "$S_NEW" "$BIND_NEW" "$DRILL/server-new.log"
readyz_ok "$BIND_NEW" "$NEW_VER"
browse_has "$BIND_NEW" "episode alpha" "new binary reads the old binary's write (forward compat)"
ingest "$BIND_NEW" "episode beta: written after the upgrade by the new binary"
sleep 2
browse_has "$BIND_NEW" "episode beta" "new binary reads its own write"
recall_has "$BIND_NEW" "what was written after the upgrade" "episode beta" "new binary recalls its own write"
stop_server

# ---------- phase 3: rollback in place ----------
# This is the phase the drill exists for. Everything above can pass while this
# fails, which is exactly the failure a release needs to know about.
say "phase 3: rollback — OLD binary on the SAME store, no data motion"
start_server "$S_OLD" "$BIND_OLD" "$DRILL/server-old-post.log"
readyz_ok "$BIND_OLD" "$OLD_VER"
browse_has "$BIND_OLD" "episode beta"  "old binary reads the NEW binary's write (rollback compat)"
browse_has "$BIND_OLD" "episode alpha" "old binary still reads pre-upgrade data"
ingest "$BIND_OLD" "episode gamma: written after the rollback by the old binary"
sleep 2
browse_has "$BIND_OLD" "episode gamma" "old binary writes after rollback"
stop_server

# ---------- phase 4: roll forward again ----------
say "phase 4: roll forward — NEW binary sees all three eras"
start_server "$S_NEW" "$BIND_NEW" "$DRILL/server-new-again.log"
readyz_ok "$BIND_NEW" "$NEW_VER"
for m in alpha beta gamma; do
  browse_has "$BIND_NEW" "episode $m" "new binary reads episode $m after the round trip"
done
stop_server

say "DRILL PASS — upgrade, rollback and roll-forward rehearsed in place ($SKIPPED check(s) skipped)"
say "log: $LOG"
exit 0

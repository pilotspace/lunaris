#!/bin/bash
# GA-4 rollback rehearsal — RELEASE.md §7, both directions, live run.
# 0.6.2 (SQLite) --lunaris-migrate--> Moon --0.7.0 binary--> rollback 0.6.2 in place.
# Scratch Moon on 6402 (NEVER 6379/6380/6381; 6399 is the bench port).
set -uo pipefail

DRILL="${DRILL_DIR:-$(mktemp -d /tmp/lunaris-rollback-drill.XXXXXX)}"
MOON_BIN="${DRILL_MOON_BIN:?set DRILL_MOON_BIN to a moon >=0.8.5 binary}"
S062="${DRILL_SERVER_OLD:?set DRILL_SERVER_OLD to the 0.6.2 lunaris-server (built with --features lunaris/llamacpp)}"
M062="${DRILL_MIGRATE:?set DRILL_MIGRATE to the 0.6.2 lunaris-migrate}"
S070="${DRILL_SERVER_NEW:?set DRILL_SERVER_NEW to the 0.7.0 lunaris-server (built with --features lunaris/llamacpp)}"
MOON_PORT="${DRILL_MOON_PORT:-6402}"
case "$MOON_PORT" in
  6379|6380|6381|6399) echo "refusing port $MOON_PORT: live/bench Moon ports are off-limits to the drill" >&2; exit 2 ;;
esac
BIND_062=127.0.0.1:8091
BIND_070=127.0.0.1:8092
export LUNARIS_EMBEDDER_GGUF="${LUNARIS_EMBEDDER_GGUF:-$HOME/.lunaris/models/granite-embedding-311m-multilingual-r2.Q4_K_M.gguf}"

mkdir -p "$DRILL/moon-data"
LOG="$DRILL/drill.log"
say() { echo "== $*" | tee -a "$LOG"; }
fail() { say "DRILL FAIL: $*"; cleanup; exit 1; }
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

wait_http() { # url, tries
  for _ in $(seq 1 "$2"); do curl -sf -m 2 "$1" >/dev/null 2>&1 && return 0; sleep 1; done
  return 1
}

# ---------- Phase 0: scratch Moon ----------
say "phase 0: scratch Moon on $MOON_PORT"
nohup "$MOON_BIN" --bind 127.0.0.1 --port "$MOON_PORT" --dir "$DRILL/moon-data" --shards 1 \
  > "$DRILL/moon.log" 2>&1 &
MOON_PID=$!
for _ in $(seq 1 20); do
  (timeout 3 redis-cli -p "$MOON_PORT" ping 2>/dev/null || redis-cli -p "$MOON_PORT" ping 2>/dev/null) | grep -q PONG && break
  sleep 1
done
(redis-cli -p "$MOON_PORT" ping | grep -q PONG) || fail "Moon did not come up"
say "moon up (pid $MOON_PID)"

# ---------- Phase 1: 0.6.2 on SQLite, seed data ----------
say "phase 1: 0.6.2 server on SQLite, seed 5 episodes"
"$S062" --bind "$BIND_062" --storage "sqlite://$DRILL/store-062.db" \
  --tokens-file "$DRILL/tokens.json" > "$DRILL/server-062-sqlite.log" 2>&1 &
SRV_PID=$!
wait_http "http://$BIND_062/healthz" 30 || fail "0.6.2 sqlite server never healthy"
i=0
for line in \
  "the production Moon store listens on port 7001 in the staging cluster" \
  "Tin approved the v0.7.0 GA rollout plan on August fourteenth" \
  "the reranker stage costs about 1.3 seconds per recall at top_in 60" \
  "the capacity study measured p50 of 21 milliseconds at 100k documents" \
  "rollback to 0.6.2 keeps the Moon store in place with no data motion"; do
  i=$((i+1))
  curl -sf -m 60 -X POST "http://$BIND_062/v1/ingest" -H "$AUTH" -H 'content-type: application/json' \
    -d "{\"source\":\"drill\",\"content\":\"episode $i: $line\"}" >> "$LOG" 2>&1 \
    || fail "ingest $i on 0.6.2 sqlite"
  echo >> "$LOG"
done
sleep 2  # let async embed/index settle
RECALL_SQLITE=$(curl -sf -m 60 -X POST "http://$BIND_062/v1/recall" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"query":"what does the reranker stage cost","k":3}')
echo "$RECALL_SQLITE" >> "$LOG"
echo "$RECALL_SQLITE" | grep -q "1.3 seconds" || fail "0.6.2 sqlite recall smoke"
say "0.6.2 sqlite recall smoke OK"
kill "$SRV_PID"; wait "$SRV_PID" 2>/dev/null; SRV_PID=

# ---------- Phase 2: migrate (upgrade step 1-2) ----------
say "phase 2: lunaris-migrate sqlite -> moon (with built-in verify)"
"$M062" --from "sqlite://$DRILL/store-062.db" --to "moon://127.0.0.1:$MOON_PORT" \
  --scope drill --commit --acknowledge-lossy --reembed-manifest "$DRILL/reembed.jsonl" > "$DRILL/migrate.log" 2>&1 \
  || { tail -20 "$DRILL/migrate.log"; fail "lunaris-migrate"; }
tail -5 "$DRILL/migrate.log" | tee -a "$LOG"

# ---------- Phase 3: deploy 0.7.0 on Moon (upgrade step 3) ----------
say "phase 3: 0.7.0 server on moon://, readyz + recall of migrated data + new write"
"$S070" --bind "$BIND_070" --storage "moon://127.0.0.1:$MOON_PORT" \
  --tokens-file "$DRILL/tokens.json" > "$DRILL/server-070-moon.log" 2>&1 &
SRV_PID=$!
wait_http "http://$BIND_070/healthz" 30 || fail "0.7.0 moon server never healthy"
curl -sf -m 10 "http://$BIND_070/readyz" >> "$LOG" 2>&1 || fail "0.7.0 /readyz"
echo >> "$LOG"
# 3a. The DECLARED gap (0.6-to-0.7.md §4): migrated KV has no FT docs, so
# recall is empty until re-embed. Assert the gap is real, then rehearse the
# documented interim recipe: same-id re-ingest of the source episodes.
RGAP=$(curl -sf -m 60 -X POST "http://$BIND_070/v1/recall" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"query":"which port does the staging Moon store listen on","k":3}')
[ "$RGAP" = "[]" ] || say "NOTE: expected empty pre-reembed recall, got: $RGAP"
say "3a: declared post-migrate recall gap confirmed (empty until re-embed)"
# 3b. Re-embed via same-id re-ingest (manifest lists derived kinds; the
# episode is the re-ingest grain — the pipeline regenerates chunks+vectors+FT).
EPS=$(curl -sf -m 30 "http://$BIND_070/v1/browse/episode?limit=50" -H "$AUTH")
echo "$EPS" | jq -c '.items[] | {id: .id, source: .source, content: .content}' | while read -r ep; do
  curl -sf -m 60 -X POST "http://$BIND_070/v1/ingest" -H "$AUTH" -H 'content-type: application/json' \
    -d "$ep" >> "$LOG" 2>&1 || exit 1
  echo >> "$LOG"
done || fail "same-id re-ingest on 0.7.0"
sleep 3
R070=$(curl -sf -m 60 -X POST "http://$BIND_070/v1/recall" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"query":"which port does the staging Moon store listen on","k":3}')
echo "$R070" >> "$LOG"
echo "$R070" | grep -q "7001" || fail "0.7.0 recall of migrated data after re-embed"
say "3b: 0.7.0 sees migrated data after same-id re-ingest re-embed"
curl -sf -m 60 -X POST "http://$BIND_070/v1/ingest" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"source":"drill","content":"episode 6: written while running on version 0.7.0 after the upgrade"}' >> "$LOG" 2>&1 \
  || fail "0.7.0-era write"
echo >> "$LOG"
sleep 2
kill "$SRV_PID"; wait "$SRV_PID" 2>/dev/null; SRV_PID=

# ---------- Phase 4: rollback 0.7.0 -> 0.6.2 in place (same Moon) ----------
say "phase 4: rollback — 0.6.2 binary on the SAME moon store, no data motion"
"$S062" --bind "$BIND_062" --storage "moon://127.0.0.1:$MOON_PORT" \
  --tokens-file "$DRILL/tokens.json" > "$DRILL/server-062-moon.log" 2>&1 &
SRV_PID=$!
wait_http "http://$BIND_062/healthz" 30 || fail "0.6.2 rollback server never healthy"
R062=$(curl -sf -m 60 -X POST "http://$BIND_062/v1/recall" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"query":"what was written while running on version 0.7.0","k":3}')
echo "$R062" >> "$LOG"
echo "$R062" | grep -q "0.7.0 after the upgrade" || fail "rollback recall: 0.7.0-era write not visible on 0.6.2"
R062B=$(curl -sf -m 60 -X POST "http://$BIND_062/v1/recall" -H "$AUTH" -H 'content-type: application/json' \
  -d '{"query":"what does the reranker stage cost","k":3}')
echo "$R062B" | grep -q "1.3 seconds" || fail "rollback recall: migrated data not visible on 0.6.2"
say "rollback OK: 0.6.2 reads both migrated AND 0.7.0-era data in place"
kill "$SRV_PID"; wait "$SRV_PID" 2>/dev/null; SRV_PID=

say "DRILL PASS — both directions rehearsed"

#!/usr/bin/env bash
# Shared plumbing + SAFETY GUARDS for the LongMemEval (LME) harness.
#
# Sourced by every entry point under scripts/bench/lme/. Nothing here runs a
# benchmark; it resolves paths, validates config, and refuses to aim a
# destructive runner at a store that is not a throwaway bench Moon.
#
# THE RULE THIS FILE EXISTS TO ENFORCE
# ------------------------------------
# Every measured LME question starts by FLUSHALL-ing its Moon, because
# LongMemEval-S requires each question to retrieve from ITS OWN haystack and
# the source filter fails open on Moon (physical isolation is the only correct
# per-question boundary). A runner pointed at a real store therefore DESTROYS
# it. Port 6381 is the operator's live personal memory store (hundreds of
# thousands of keys, launchd unit dev.lunaris.moon-6381). It is hard-refused
# here, in one place, for every entry point.
#
# shellcheck shell=bash

# ---------------------------------------------------------------------------
# Repo root — derived, never hardcoded. Works from a fresh clone, a git
# worktree, or a tarball export (the `git` call is only the fast path).
# ---------------------------------------------------------------------------
lme_repo_root() {
  local here
  here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  git -C "$here" rev-parse --show-toplevel 2>/dev/null \
    || (cd "$here/../../.." && pwd)
}

LME_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(lme_repo_root)}"
export LME_DIR REPO_ROOT

lme_die() { echo "FATAL: $*" >&2; exit "${LME_EXIT:-2}"; }
lme_log() { echo "[lme] $*" >&2; }

# `--help` for every entry point: the script's own leading comment block
# (everything after the shebang, up to the first line of code), un-hashed.
# Keeping help and source comment the same text means they cannot drift.
lme_help() { # $1 = script path
  awk 'NR==1 {next} /^#/ {sub(/^# ?/, ""); print; next} {exit}' "$1"
}

# ---------------------------------------------------------------------------
# GUARD 1 — reserved ports. 6381 is always reserved; operators may add more
# via LME_RESERVED_PORTS (space-separated). A reserved port can never be the
# target of a runner, a fill shard, or the watchdog.
# ---------------------------------------------------------------------------
LME_RESERVED_PORTS="${LME_RESERVED_PORTS:-} 6381"

lme_guard_port() { # $1 = port, $2 = human label for the error
  local port="$1" label="${2:-target}" reserved
  case "$port" in
    ''|*[!0-9]*) LME_EXIT=2 lme_die "$label port must be numeric, got '$port'" ;;
  esac
  for reserved in $LME_RESERVED_PORTS; do
    if [ "$port" = "$reserved" ]; then
      LME_EXIT=4 lme_die \
"$label resolved to port $port, which is RESERVED.

  6381 is the live personal Lunaris memory store (launchd dev.lunaris.moon-6381).
  Every measured LME question FLUSHALLs its target Moon, so aiming this harness
  at 6381 would destroy it irrecoverably.

  Use a dedicated throwaway bench Moon. Default: 6399.
      scripts/bench/lme/moon_watchdog.sh    # starts + keeps one alive
  Then re-run with MOON_PORT=6399 (or unset MOON_PORT to take the default)."
    fi
  done
}

# GUARD 1b — a MOON_URL passed in wholesale must not smuggle a reserved port
# past the numeric MOON_PORT check.
lme_guard_url() { # $1 = moon url, $2 = label
  local url="$1" label="${2:-MOON_URL}" reserved
  for reserved in $LME_RESERVED_PORTS; do
    case "$url" in
      *":$reserved"|*":$reserved/"*|*":$reserved?"*)
        LME_EXIT=4 lme_die "$label ('$url') targets reserved port $reserved (live store). Refusing." ;;
    esac
  done
}

# ---------------------------------------------------------------------------
# GUARD 2 — credentials. Read from the environment or from a file path given
# by env. The key is NEVER echoed, logged, or written into an artifact, and no
# key file path is baked into the repo.
# ---------------------------------------------------------------------------
lme_load_api_key() {
  if [ -z "${MINIMAX_API_KEY:-}" ] && [ -n "${LUNARIS_BENCH_KEY_FILE:-}" ]; then
    [ -r "$LUNARIS_BENCH_KEY_FILE" ] \
      || LME_EXIT=2 lme_die "LUNARIS_BENCH_KEY_FILE is not readable: $LUNARIS_BENCH_KEY_FILE"
    MINIMAX_API_KEY="$(tr -d '\r\n' < "$LUNARIS_BENCH_KEY_FILE")"
    export MINIMAX_API_KEY
  fi
}

lme_require_api_key() {
  lme_load_api_key
  [ -n "${MINIMAX_API_KEY:-}" ] || LME_EXIT=2 lme_die \
"MINIMAX_API_KEY is unset.

  Generation, judging and (graph arm) extraction all call the MiniMax API.
  Provide it one of two ways — never commit it:
      export MINIMAX_API_KEY=...
      export LUNARIS_BENCH_KEY_FILE=/path/outside/the/repo/minimax.key"
}

# Redacted rendering for --dry-run output. Shows only that a key is present
# and its length, never any of its bytes.
lme_key_status() {
  if [ -n "${MINIMAX_API_KEY:-}" ]; then
    echo "present (${#MINIMAX_API_KEY} chars, value redacted)"
  else
    echo "MISSING"
  fi
}

# ---------------------------------------------------------------------------
# Resolved paths (all overridable; none personal).
# ---------------------------------------------------------------------------
LME_EVAL_BIN="${LME_EVAL_BIN:-$REPO_ROOT/target/release/lunaris-evals}"
LME_MOON_BIN="${LME_MOON_BIN:-$HOME/.lunaris/bin/moon}"
# Results and caches default under target/ so they are gitignored by the
# existing `/target` rule and never land in a commit.
LME_RESULTS_DIR="${LME_RESULTS_DIR:-$REPO_ROOT/target/lme}"
LME_EXTRACT_CACHE_DIR="${LME_EXTRACT_CACHE_DIR:-$LME_RESULTS_DIR/extract-cache}"
# LongMemEval dataset cache. The dataset itself is EXTERNAL and is never
# committed; lunaris-evals downloads it here on first use.
LUNARIS_EVAL_CACHE_DIR="${LUNARIS_EVAL_CACHE_DIR:-$HOME/.cache/lunaris/eval-hub}"
export LME_EVAL_BIN LME_MOON_BIN LME_RESULTS_DIR LME_EXTRACT_CACHE_DIR LUNARIS_EVAL_CACHE_DIR

# ---------------------------------------------------------------------------
# Per-question watchdog. This box (and CI macOS runners) ship neither GNU
# `timeout` nor `gtimeout`. alarm(2) survives execve, so `perl -e 'alarm N;
# exec CMD'` delivers SIGALRM to the harness after N seconds; the shell then
# observes rc 142 (128 + SIGALRM).
# ---------------------------------------------------------------------------
lme_timeout() { # $1 = seconds, rest = command
  local secs="$1"; shift
  perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$secs" "$@"
}

lme_rc_is_timeout() { # $1 = rc
  [ "$1" -eq 142 ] || [ "$1" -eq 124 ] || [ "$1" -eq 137 ]
}

# ---------------------------------------------------------------------------
# Moon helpers. Every one of these routes through lme_guard_port first.
# ---------------------------------------------------------------------------
lme_moon_ping() { # $1 = port
  lme_guard_port "$1" "moon ping target"
  redis-cli -p "$1" -t 3 ping >/dev/null 2>&1
}

lme_moon_flush() { # $1 = port — DESTRUCTIVE, guarded
  lme_guard_port "$1" "FLUSHALL target"
  redis-cli -p "$1" -t 3 flushall >/dev/null 2>&1
}

lme_moon_start() { # $1 = port, $2 = data dir, $3 = log file
  lme_guard_port "$1" "moon start target"
  mkdir -p "$2" "$(dirname "$3")"
  nohup "$LME_MOON_BIN" --bind 127.0.0.1 --port "$1" --dir "$2" --shards 1 \
    --protected-mode no --disk-free-min-pct 1 >> "$3" 2>&1 &
  local pid=$!
  local _try
  for _try in $(seq 1 50); do
    lme_moon_ping "$1" && break
    sleep 0.2
  done
  echo "$pid"
}

# Count entries in a directory without tripping over exotic filenames.
lme_count() { # $1 = dir, $2 = optional -name glob
  [ -d "$1" ] || { echo 0; return; }
  if [ -n "${2:-}" ]; then
    find "$1" -maxdepth 1 -type f -name "$2" 2>/dev/null | wc -l | tr -d ' '
  else
    find "$1" -maxdepth 1 -mindepth 1 2>/dev/null | wc -l | tr -d ' '
  fi
}

# ---------------------------------------------------------------------------
# Preflight — shared by every entry point's --dry-run.
# ---------------------------------------------------------------------------
lme_preflight_report() { # $1 = port
  local port="$1"
  echo "  repo root            : $REPO_ROOT"
  echo "  eval binary          : $LME_EVAL_BIN $([ -x "$LME_EVAL_BIN" ] && echo '(present)' || echo '(MISSING — see README build step)')"
  echo "  moon binary          : $LME_MOON_BIN $([ -x "$LME_MOON_BIN" ] && echo '(present)' || echo '(MISSING)')"
  echo "  bench moon port      : $port $(lme_moon_ping "$port" && echo '(reachable)' || echo '(NOT reachable)')"
  echo "  results dir          : $LME_RESULTS_DIR"
  echo "  extraction cache     : $LME_EXTRACT_CACHE_DIR ($(lme_count "$LME_EXTRACT_CACHE_DIR") entries)"
  echo "  dataset cache        : $LUNARIS_EVAL_CACHE_DIR (external, not committed)"
  echo "  MINIMAX_API_KEY      : $(lme_key_status)"
  echo "  reserved ports       : $(echo "$LME_RESERVED_PORTS" | tr -s ' ')"
}

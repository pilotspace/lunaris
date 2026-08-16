#!/usr/bin/env bash
# Shared plumbing + SAFETY GUARDS for the PersonaMem (PM) harness.
#
# Sourced by every entry point under scripts/bench/pm/. Nothing here runs a
# benchmark; it resolves paths, validates config, and refuses to aim a
# destructive runner at a store that is not a throwaway bench Moon.
#
# THE RULE THIS FILE EXISTS TO ENFORCE
# ------------------------------------
# Every measured PersonaMem shared-context starts by FLUSHALL-ing its Moon,
# because each persona must be answered from ITS OWN interaction history and
# physical isolation is the only sound boundary. A runner pointed at a real
# store therefore DESTROYS it. Three ports are hard-refused here, in one place,
# for every entry point:
#   6379  the conventional Redis port (someone's local Redis)
#   6380  the ai-proxy Redis (NOT a Moon)
#   6381  the operator's live personal Lunaris memory store
#         (launchd dev.lunaris.moon-6381, hundreds of thousands of keys)
# The dedicated throwaway bench Moon is 6399.
#
# shellcheck shell=bash

# ---------------------------------------------------------------------------
# Repo root — derived, never hardcoded. Works from a fresh clone, a git
# worktree, or a tarball export (the `git` call is only the fast path).
# ---------------------------------------------------------------------------
pm_repo_root() {
  local here
  here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  git -C "$here" rev-parse --show-toplevel 2>/dev/null \
    || (cd "$here/../../.." && pwd)
}

PM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(pm_repo_root)}"
export PM_DIR REPO_ROOT

pm_die() { echo "FATAL: $*" >&2; exit "${PM_EXIT:-2}"; }
pm_log() { echo "[pm] $*" >&2; }

# `--help` for every entry point: the script's own leading comment block
# (everything after the shebang, up to the first line of code), un-hashed.
# Keeping help and source comment the same text means they cannot drift.
pm_help() { # $1 = script path
  awk 'NR==1 {next} /^#/ {sub(/^# ?/, ""); print; next} {exit}' "$1"
}

# ---------------------------------------------------------------------------
# GUARD 1 — reserved ports. Operators may add more via PM_RESERVED_PORTS
# (space-separated). A reserved port can never be the target of a runner or
# the watchdog.
# ---------------------------------------------------------------------------
PM_RESERVED_PORTS="${PM_RESERVED_PORTS:-} 6379 6380 6381"

pm_guard_port() { # $1 = port, $2 = human label for the error
  local port="$1" label="${2:-target}" reserved
  case "$port" in
    ''|*[!0-9]*) PM_EXIT=2 pm_die "$label port must be numeric, got '$port'" ;;
  esac
  for reserved in $PM_RESERVED_PORTS; do
    if [ "$port" = "$reserved" ]; then
      PM_EXIT=4 pm_die \
"$label resolved to port $port, which is RESERVED.

  6379 = conventional Redis, 6380 = the ai-proxy Redis, 6381 = the live
  personal Lunaris memory store (launchd dev.lunaris.moon-6381).
  Every measured PersonaMem context FLUSHALLs its target Moon, so aiming this
  harness at any of them would destroy data irrecoverably.

  Use a dedicated throwaway bench Moon. Default: 6399.
      scripts/bench/lme/moon_watchdog.sh    # starts + keeps one alive on 6399
  Then re-run with MOON_PORT=6399 (or unset MOON_PORT to take the default)."
    fi
  done
}

# GUARD 1b — a MOON_URL passed in wholesale must not smuggle a reserved port
# past the numeric MOON_PORT check.
pm_guard_url() { # $1 = moon url, $2 = label
  local url="$1" label="${2:-MOON_URL}" reserved
  for reserved in $PM_RESERVED_PORTS; do
    case "$url" in
      *":$reserved"|*":$reserved/"*|*":$reserved?"*)
        PM_EXIT=4 pm_die "$label ('$url') targets reserved port $reserved (live store). Refusing." ;;
    esac
  done
}

# ---------------------------------------------------------------------------
# GUARD 2 — credentials. The default reader (claude-sonnet-5) is served by the
# local Ollama-shaped chat bridge and needs NO key. A MiniMax-named reader
# routes through the native MiniMax client instead and DOES. Fail early with
# the reason rather than after the first question errors out. The key is never
# echoed, logged, or written into an artifact.
# ---------------------------------------------------------------------------
pm_load_api_key() {
  if [ -z "${MINIMAX_API_KEY:-}" ] && [ -n "${LUNARIS_BENCH_KEY_FILE:-}" ]; then
    [ -r "$LUNARIS_BENCH_KEY_FILE" ] \
      || PM_EXIT=2 pm_die "LUNARIS_BENCH_KEY_FILE is not readable: $LUNARIS_BENCH_KEY_FILE"
    MINIMAX_API_KEY="$(tr -d '\r\n' < "$LUNARIS_BENCH_KEY_FILE")"
    export MINIMAX_API_KEY
  fi
}

pm_require_reader_credentials() { # $1 = reader model
  pm_load_api_key
  case "$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')" in
    *minimax*)
      [ -n "${MINIMAX_API_KEY:-}" ] || PM_EXIT=2 pm_die \
"READER_MODEL='$1' is a MiniMax model but MINIMAX_API_KEY is unset.

  Provide it one of two ways — never commit it:
      export MINIMAX_API_KEY=...
      export LUNARIS_BENCH_KEY_FILE=/path/outside/the/repo/minimax.key
  Or pick a reader served by the chat bridge (default: claude-sonnet-5)." ;;
  esac
}

pm_key_status() {
  if [ -n "${MINIMAX_API_KEY:-}" ]; then
    echo "present (${#MINIMAX_API_KEY} chars, value redacted)"
  else
    echo "absent (not needed for a bridge-served reader)"
  fi
}

# ---------------------------------------------------------------------------
# Resolved paths (all overridable; none personal).
# ---------------------------------------------------------------------------
PM_EVAL_BIN="${PM_EVAL_BIN:-$REPO_ROOT/target/release/lunaris-evals}"
PM_MOON_BIN="${PM_MOON_BIN:-${LME_MOON_BIN:-$HOME/.lunaris/bin/moon}}"
# Results default under target/ so they are gitignored by the existing
# `/target` rule and never land in a commit.
PM_RESULTS_DIR="${PM_RESULTS_DIR:-$REPO_ROOT/target/pm}"
# PersonaMem dataset cache. The dataset itself is EXTERNAL and is never
# committed; lunaris-evals downloads it here on first use.
LUNARIS_EVAL_CACHE_DIR="${LUNARIS_EVAL_CACHE_DIR:-$HOME/.cache/lunaris/eval-hub}"
export PM_EVAL_BIN PM_MOON_BIN PM_RESULTS_DIR LUNARIS_EVAL_CACHE_DIR

# ---------------------------------------------------------------------------
# Per-context watchdog. This box (and CI macOS runners) ship neither GNU
# `timeout` nor `gtimeout`. alarm(2) survives execve, so `perl -e 'alarm N;
# exec CMD'` delivers SIGALRM to the harness after N seconds; the shell then
# observes rc 142 (128 + SIGALRM).
# ---------------------------------------------------------------------------
pm_timeout() { # $1 = seconds, rest = command
  local secs="$1"; shift
  perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$secs" "$@"
}

pm_rc_is_timeout() { # $1 = rc
  [ "$1" -eq 142 ] || [ "$1" -eq 124 ] || [ "$1" -eq 137 ]
}

# ---------------------------------------------------------------------------
# Moon helpers. Every one of these routes through pm_guard_port first.
# ---------------------------------------------------------------------------
pm_moon_ping() { # $1 = port
  pm_guard_port "$1" "moon ping target"
  redis-cli -p "$1" -t 3 ping >/dev/null 2>&1
}

pm_moon_flush() { # $1 = port — DESTRUCTIVE, guarded
  pm_guard_port "$1" "FLUSHALL target"
  redis-cli -p "$1" -t 3 flushall >/dev/null 2>&1
}

pm_count() { # $1 = dir, $2 = optional -name glob
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
pm_preflight_report() { # $1 = port
  local port="$1"
  echo "  repo root            : $REPO_ROOT"
  echo "  eval binary          : $PM_EVAL_BIN $([ -x "$PM_EVAL_BIN" ] && echo '(present)' || echo '(MISSING — see README build step)')"
  echo "  moon binary          : $PM_MOON_BIN $([ -x "$PM_MOON_BIN" ] && echo '(present)' || echo '(MISSING)')"
  echo "  bench moon port      : $port $(pm_moon_ping "$port" && echo '(reachable)' || echo '(NOT reachable)')"
  echo "  results dir          : $PM_RESULTS_DIR"
  echo "  dataset cache        : $LUNARIS_EVAL_CACHE_DIR (external, not committed)"
  echo "  MINIMAX_API_KEY      : $(pm_key_status)"
  echo "  reserved ports       : $(echo "$PM_RESERVED_PORTS" | tr -s ' ')"
}

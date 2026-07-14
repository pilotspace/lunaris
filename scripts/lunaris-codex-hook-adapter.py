#!/usr/bin/env python3
"""Normalize agent hook stdin for Lunaris capture and context injection.

Codex 0.134 uses Claude-compatible hook event names for configuration, but it
also has Codex-only events that older lunaris-hook releases do not parse. This
adapter keeps the hook path usable by translating those events into the four
envelopes lunaris-hook already captures: SessionStart, PreToolUse, PostToolUse,
and Stop.

For Claude Code, the same adapter can emit `hookSpecificOutput.additionalContext`
so prompt and post-tool recalls are injected into Claude's context window.

Modes:
  capture    capture the event through lunaris-hook
  inject     recall prompt memories through lunaris-contextd and emit context
  post-tool  capture a tool result, then recall smaller follow-up context
  feedback   record turn feedback
  auto       capture plus injection for prompt/post-tool events
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import socket
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
LUNARIS_HOOK = ROOT / "target" / "release" / "lunaris-hook"
LUNARIS_CONTEXTD = ROOT / "target" / "release" / "lunaris-contextd"
KNOWN_DIRECT = {"SessionStart", "PreToolUse", "PostToolUse", "Stop"}
PROMPT_EVENTS = {"userpromptsubmit", "userpromptexpansion"}
POST_TOOL_EVENTS = {"posttooluse", "postcompact", "subagentstop"}
SESSION_START_EVENTS = {"sessionstart"}
LOW_VALUE_TOOLS = {"pwd", "date", "ls"}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--mode",
        choices=("capture", "inject", "post-tool", "feedback", "session-start", "auto"),
        default=os.environ.get("LUNARIS_CODEX_HOOK_MODE", "capture"),
    )
    parser.add_argument(
        "--target",
        choices=("codex", "claude"),
        default=os.environ.get("LUNARIS_HOOK_OUTPUT_TARGET", "codex"),
        help="Output format for context injection.",
    )
    args = parser.parse_args()

    raw = sys.stdin.buffer.read()
    if not raw.strip():
        return 0

    try:
        event = json.loads(raw)
    except json.JSONDecodeError:
        sys.stderr.write("lunaris-codex-hook-adapter: invalid JSON on stdin\n")
        return 64

    if not isinstance(event, dict):
        sys.stderr.write("lunaris-codex-hook-adapter: hook payload must be an object\n")
        return 64

    if env_disabled("LUNARIS_CONTEXT_ENABLED", "LUNARIS_CODEX_CONTEXT_ENABLED"):
        return run_capture(event)

    if args.mode == "capture":
        return run_capture(event)
    if args.mode == "inject":
        return run_prompt_injection(event, args.target)
    if args.mode == "post-tool":
        return run_post_tool_injection(event, args.target)
    if args.mode == "feedback":
        return run_feedback(event)
    if args.mode == "session-start":
        return run_session_digest(event, args.target)
    if args.mode == "auto":
        capture_code = run_capture(event)
        kind = compact_kind(event)
        if kind in PROMPT_EVENTS:
            inject_code = run_prompt_injection(event, args.target)
        elif kind in POST_TOOL_EVENTS:
            inject_code = run_post_tool_injection(event, args.target)
        elif kind in SESSION_START_EVENTS:
            inject_code = run_session_digest(event, args.target)
        else:
            inject_code = 0
        return capture_code if capture_code != 0 else inject_code
    return 0


# Content fields that make a hook event worth persisting. `cwd` is deliberately
# NOT here — every envelope carries it, so it is metadata, not signal.
CAPTURE_CONTENT_FIELDS = (
    "error",
    "stderr",
    "output",
    "result",
    "tool_response",
    "toolResponse",
    "tool_input",
    "toolInput",
    "message",
    "content",
    "prompt",
    "command",
    "patch",
    "new_string",
    "diff",
)


def _capture_gate_enabled() -> bool:
    """The signal-gate is on unless LUNARIS_CONTEXT_CAPTURE_GATE=off (kill-switch)."""
    return os.environ.get("LUNARIS_CONTEXT_CAPTURE_GATE", "on").strip().lower() != "off"


def capture_has_signal(event: Any) -> bool:
    """Fail-OPEN predicate: True = worth persisting as a durable memory.

    Drops metadata-only envelopes ({cwd, duration_ms, permission_mode, prompt_id}
    with no tool body) and LOW_VALUE_TOOLS ({pwd, date, ls}). Keeps anything that
    carries a touched path or a non-empty content field. Any non-dict event or
    unexpected error returns True — we never silently drop a capture on uncertainty.
    """
    try:
        if not isinstance(event, dict):
            return True  # fail open
        if extract_tool_name(event).lower() in LOW_VALUE_TOOLS:
            return False
        # `cwd` is on every envelope; strip it so it is not mistaken for signal.
        scan = {k: v for k, v in event.items() if k != "cwd"}
        if extract_paths(scan):
            return True
        for key in CAPTURE_CONTENT_FIELDS:
            value = event.get(key)
            if isinstance(value, str) and value.strip():
                return True
            if isinstance(value, (dict, list)) and value:
                return True
        return False
    except Exception:
        return True  # fail open — capture on any uncertainty


def run_capture(event: dict[str, Any]) -> int:
    # Signal-gate: only high-value events become durable memories. A dropped
    # event is success (return 0), never a non-zero hook exit that blocks the
    # tool. Bypass with LUNARIS_CONTEXT_CAPTURE_GATE=off.
    if _capture_gate_enabled() and not capture_has_signal(event):
        return 0
    if env_enabled("LUNARIS_CONTEXT_CAPTURE_FAST", default=True):
        kind = compact_kind(event)
        if kind == "pretooluse":
            payload = {
                "type": "capture_tool_call",
                "scope": hook_scope(),
                "cwd": str(event.get("cwd") or event.get("current_working_directory") or os.getcwd()),
                "session_id": session_id(event),
                "tool": extract_tool_name(event),
                "payload": event,
            }
            contextd_request(strip_none(payload), timeout_ms=env_int_any("LUNARIS_CONTEXT_CAPTURE_TIMEOUT_MS") or 120)
            return 0
        if kind == "posttooluse":
            payload = {
                "type": "capture_tool_result",
                "scope": hook_scope(),
                "cwd": str(event.get("cwd") or event.get("current_working_directory") or os.getcwd()),
                "session_id": session_id(event),
                "tool": extract_tool_name(event),
                "payload": event,
            }
            contextd_request(strip_none(payload), timeout_ms=env_int_any("LUNARIS_CONTEXT_CAPTURE_TIMEOUT_MS") or 120)
            return 0

    envelope = normalize_event(event)
    cwd = Path(envelope.get("cwd") or os.getcwd())
    if not cwd.exists():
        cwd = Path(os.getcwd())

    env = os.environ.copy()
    env.setdefault("LUNARIS_HOOK_LOG", "error")
    if "LUNARIS_STORE_URL" not in env and "LUNARIS_MCP_STORAGE" in env:
        env["LUNARIS_STORE_URL"] = env["LUNARIS_MCP_STORAGE"]

    proc = subprocess.run(
        [str(LUNARIS_HOOK)],
        input=json.dumps(envelope, separators=(",", ":")).encode("utf-8"),
        cwd=str(cwd),
        env=env,
        check=False,
    )
    return int(proc.returncode)


def run_prompt_injection(event: dict[str, Any], target: str) -> int:
    prompt = extract_prompt(event)
    if not prompt.strip():
        return 0
    request = {
        "type": "recall_for_prompt",
        "scope": hook_scope(),
        "cwd": str(event.get("cwd") or event.get("current_working_directory") or os.getcwd()),
        "session_id": session_id(event),
        "prompt": prompt,
        "max_hits": env_int_any("LUNARIS_CONTEXT_MAX_HITS", "LUNARIS_CODEX_CONTEXT_MAX_HITS"),
        "max_chars": env_int_any("LUNARIS_CONTEXT_MAX_CHARS", "LUNARIS_CODEX_CONTEXT_MAX_CHARS"),
        "min_score": env_float_any("LUNARIS_CONTEXT_MIN_SCORE", "LUNARIS_CODEX_CONTEXT_MIN_SCORE"),
    }
    response = contextd_request(
        strip_none(request),
        timeout_ms=env_int_any("LUNARIS_CONTEXT_TIMEOUT_MS", "LUNARIS_CODEX_CONTEXT_TIMEOUT_MS") or 300,
    )
    emit_context_response(response, target, claude_hook_event_name(event, "UserPromptSubmit"))
    return 0


def run_post_tool_injection(event: dict[str, Any], target: str) -> int:
    tool = extract_tool_name(event)
    if tool.lower() in LOW_VALUE_TOOLS:
        return 0
    summary = summarize_tool_event(event)
    if not summary.strip():
        return 0
    payload = {
        "type": "capture_tool_result",
        "scope": hook_scope(),
        "cwd": str(event.get("cwd") or event.get("current_working_directory") or os.getcwd()),
        "session_id": session_id(event),
        "tool": tool,
        "payload": event,
    }
    # Best effort extra capture. Existing lunaris-hook capture already runs in
    # post-tool mode, so this sidecar capture can fail silently.
    contextd_request(strip_none(payload), timeout_ms=80, autostart=True)

    request = {
        "type": "recall_after_tool",
        "scope": hook_scope(),
        "cwd": str(event.get("cwd") or event.get("current_working_directory") or os.getcwd()),
        "session_id": session_id(event),
        "tool": tool,
        "summary": summary,
        "paths": extract_paths(event),
        "max_hits": env_int_any(
            "LUNARIS_CONTEXT_POST_TOOL_MAX_HITS",
            "LUNARIS_CONTEXT_MAX_HITS",
            "LUNARIS_CODEX_POST_TOOL_MAX_HITS",
        ),
        "max_chars": env_int_any(
            "LUNARIS_CONTEXT_POST_TOOL_MAX_CHARS",
            "LUNARIS_CONTEXT_MAX_CHARS",
            "LUNARIS_CODEX_POST_TOOL_MAX_CHARS",
        ),
        "min_score": env_float_any(
            "LUNARIS_CONTEXT_POST_TOOL_MIN_SCORE",
            "LUNARIS_CONTEXT_MIN_SCORE",
            "LUNARIS_CODEX_POST_TOOL_MIN_SCORE",
        ),
    }
    response = contextd_request(
        strip_none(request),
        timeout_ms=env_int_any(
            "LUNARIS_CONTEXT_POST_TOOL_TIMEOUT_MS",
            "LUNARIS_CONTEXT_TIMEOUT_MS",
            "LUNARIS_CODEX_POST_TOOL_TIMEOUT_MS",
        )
        or 300,
    )
    emit_context_response(response, target, claude_hook_event_name(event, "PostToolUse"))
    return 0


def run_session_digest(event: dict[str, Any], target: str) -> int:
    """Inject a curated Lunaris digest of the scope's recent durable decisions
    at session start — the single-source replacement for a MEMORY.md auto-load.

    Design-for-failure: a digest is a nicety, never a gate. Any contextd error,
    timeout, or empty result still exits 0 with no injection, so a cold or down
    daemon can never block the session from starting.
    """
    request = {
        "type": "session_digest",
        "scope": hook_scope(),
        "cwd": str(event.get("cwd") or event.get("current_working_directory") or os.getcwd()),
        "session_id": session_id(event),
        "max_hits": env_int_any("LUNARIS_CONTEXT_DIGEST_MAX_HITS"),
        "max_chars": env_int_any("LUNARIS_CONTEXT_DIGEST_MAX_CHARS"),
    }
    try:
        response = contextd_request(
            strip_none(request),
            timeout_ms=env_int_any("LUNARIS_CONTEXT_DIGEST_TIMEOUT_MS", "LUNARIS_CONTEXT_TIMEOUT_MS")
            or 400,
        )
    except Exception:
        return 0
    emit_context_response(response, target, claude_hook_event_name(event, "SessionStart"))
    return 0


def run_feedback(event: dict[str, Any]) -> int:
    request = {
        "type": "turn_feedback",
        "scope": hook_scope(),
        "cwd": str(event.get("cwd") or event.get("current_working_directory") or os.getcwd()),
        "session_id": session_id(event),
        "injected_memory_ids": event.get("injected_memory_ids") or [],
        "outcome": extract_feedback(event),
    }
    contextd_request(strip_none(request), timeout_ms=80)
    return 0


def contextd_request(request: dict[str, Any], timeout_ms: int, autostart: bool = True) -> dict[str, Any]:
    socket_path = Path(
        os.environ.get("LUNARIS_CONTEXTD_SOCKET")
        or (Path.home() / ".lunaris" / "codex-contextd.sock")
    )
    payload = json.dumps(request, separators=(",", ":")).encode("utf-8")

    # Cold-start budget (ADD task contextd-cold-start-lifecycle): when THIS
    # call has to start the daemon, the first request also pays the lazy GGUF
    # embedder load — seconds, not the warm-path milliseconds. The old code
    # computed the deadline BEFORE the spawn, so the default 300ms could never
    # cover a cold start and the first prompt after boot silently got zero
    # memories. A warm daemon keeps the caller's timeout_ms unchanged.
    cold = autostart and not contextd_healthy(socket_path, timeout_ms=30)
    if cold:
        start_contextd(socket_path)
        cold_ms = env_int_any("LUNARIS_CONTEXT_COLD_TIMEOUT_MS") or 15000
        timeout_ms = max(timeout_ms, cold_ms)
    deadline = time.monotonic() + (timeout_ms / 1000.0)

    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                remaining = max(0.001, deadline - time.monotonic())
                client.settimeout(remaining)
                client.connect(str(socket_path))
                client.sendall(payload)
                client.shutdown(socket.SHUT_WR)
                chunks: list[bytes] = []
                while True:
                    chunk = client.recv(65536)
                    if not chunk:
                        break
                    chunks.append(chunk)
            if not chunks:
                return {}
            response = json.loads(b"".join(chunks))
            return response if isinstance(response, dict) else {}
        except (OSError, json.JSONDecodeError, TimeoutError) as exc:
            last_error = exc
            if autostart:
                start_contextd(socket_path)
                autostart = False
            time.sleep(0.01)
    if os.environ.get("LUNARIS_CODEX_HOOK_DEBUG") == "1" and last_error is not None:
        sys.stderr.write(f"lunaris contextd request failed: {last_error}\n")
    return {}


def socket_owner_alive(socket_path: Path) -> bool:
    """True when any live process's argv references `socket_path` — the
    mid-load daemon shape (holds the socket, can't answer health yet)."""
    try:
        proc = subprocess.run(
            ["pgrep", "-f", str(socket_path)],
            text=True,
            capture_output=True,
            check=False,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    own = str(os.getpid())
    return any(line.strip() and line.strip() != own for line in proc.stdout.splitlines())


def start_contextd(socket_path: Path) -> None:
    if os.environ.get("LUNARIS_CONTEXTD_AUTOSTART", "1") in {"0", "false", "False"}:
        return
    if not LUNARIS_CONTEXTD.exists():
        return
    socket_path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = socket_path.with_suffix(socket_path.suffix + ".lock")
    with open(lock_path, "a+b") as lock_file:
        try:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
            prune_contextd_duplicates(socket_path)
            if contextd_healthy(socket_path, timeout_ms=50):
                return
            # Liveness guard (ADD task contextd-cold-start-lifecycle): a
            # daemon that is mid-GGUF-load holds the socket but cannot answer
            # health. Unlinking its socket and spawning a duplicate here was
            # the cold-start restart storm (5 leaked daemons / 2 verify runs
            # in the 2026-07-14 deep test). If ANY live process references
            # this socket path, leave it alone — the caller's cold-start
            # budget rides out the load.
            if socket_path.exists() and socket_owner_alive(socket_path):
                return
            if socket_path.exists():
                try:
                    socket_path.unlink()
                except OSError:
                    pass
            spawn_contextd(socket_path)
            wait_for_contextd(socket_path, timeout_ms=250)
            prune_contextd_duplicates(socket_path)
        finally:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_UN)


def spawn_contextd(socket_path: Path) -> None:
    env = os.environ.copy()
    env.setdefault("LUNARIS_CONTEXTD_LOG", "error")
    default_gguf = Path.home() / ".lunaris" / "models" / "granite-embedding-311m-multilingual-r2.Q4_K_M.gguf"
    if "LUNARIS_EMBEDDER_GGUF" not in env and default_gguf.exists():
        env["LUNARIS_EMBEDDER_GGUF"] = str(default_gguf)
    subprocess.Popen(
        [str(LUNARIS_CONTEXTD), "--socket", str(socket_path)],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        cwd=str(ROOT),
        env=env,
        start_new_session=True,
    )


def contextd_healthy(socket_path: Path, timeout_ms: int) -> bool:
    if not socket_path.exists():
        return False
    deadline = time.monotonic() + (timeout_ms / 1000.0)
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(max(0.001, deadline - time.monotonic()))
            client.connect(str(socket_path))
            client.sendall(b'{"type":"health"}')
            client.shutdown(socket.SHUT_WR)
            data = client.recv(4096)
        if not data:
            return False
        response = json.loads(data)
        return isinstance(response, dict) and response.get("ok") is True
    except (OSError, json.JSONDecodeError, TimeoutError):
        return False


def wait_for_contextd(socket_path: Path, timeout_ms: int) -> bool:
    deadline = time.monotonic() + (timeout_ms / 1000.0)
    while time.monotonic() < deadline:
        if contextd_healthy(socket_path, timeout_ms=30):
            return True
        time.sleep(0.01)
    return False


def prune_contextd_duplicates(socket_path: Path) -> None:
    """Keep only the newest contextd for this socket.

    A burst of hook subprocesses can race during sidecar autostart. Older
    contextd processes may stay alive on unlinked Unix sockets after a later
    starter rebinds the visible path. They are idle but expensive because each
    one can hold model memory. Prune them opportunistically from the adapter.
    """
    try:
        proc = subprocess.run(
            ["pgrep", "-f", f"{LUNARIS_CONTEXTD} --socket {socket_path}"],
            text=True,
            capture_output=True,
            check=False,
        )
    except OSError:
        return
    pids: list[int] = []
    for line in proc.stdout.splitlines():
        try:
            pid = int(line.strip())
        except ValueError:
            continue
        if pid != os.getpid():
            pids.append(pid)
    if len(pids) <= 1:
        return
    for pid in sorted(pids)[:-1]:
        try:
            os.kill(pid, 15)
        except OSError:
            pass


def emit_context_response(response: dict[str, Any], target: str, hook_event_name: str) -> None:
    text = str(response.get("rendered_context") or "")
    if not text.strip():
        return
    if target == "claude":
        json.dump(
            {
                "hookSpecificOutput": {
                    "hookEventName": hook_event_name,
                    "additionalContext": text,
                }
            },
            sys.stdout,
            separators=(",", ":"),
        )
        sys.stdout.write("\n")
        return
    # Codex's generated schema exposes HookOutputEntry {kind,text} and
    # HookOutputEntryKind includes "context". Emit a JSON array so multiple
    # hook entries can be represented without changing the command contract.
    json.dump([{"kind": "context", "text": text}], sys.stdout, separators=(",", ":"))
    sys.stdout.write("\n")


def normalize_event(event: dict[str, Any]) -> dict[str, Any]:
    codex_kind = str(
        event.get("hook_event_name")
        or event.get("event_name")
        or event.get("eventName")
        or event.get("event")
        or event.get("type")
        or "UnknownCodexHook"
    )
    claude_kind = to_claude_kind(codex_kind)
    session_id = str(
        event.get("session_id")
        or event.get("thread_id")
        or event.get("threadId")
        or event.get("conversation_id")
        or event.get("conversationId")
        or "codex-session"
    )
    cwd = str(event.get("cwd") or event.get("current_working_directory") or os.getcwd())
    timestamp = str(event.get("timestamp") or datetime.now(timezone.utc).isoformat())
    transcript_path = event.get("transcript_path") or event.get("agent_transcript_path")
    event_id = str(event.get("event_id") or stable_event_id(session_id, codex_kind, event))

    if claude_kind in KNOWN_DIRECT and codex_kind == claude_kind:
        envelope = dict(event)
        envelope.setdefault("session_id", session_id)
        envelope.setdefault("cwd", cwd)
        envelope.setdefault("event_id", event_id)
        envelope.setdefault("timestamp", timestamp)
        if transcript_path is not None:
            envelope.setdefault("transcript_path", transcript_path)
        return envelope

    base: dict[str, Any] = {
        "hook_event_name": claude_kind,
        "session_id": session_id,
        "cwd": cwd,
        "event_id": event_id,
        "timestamp": timestamp,
    }
    if transcript_path is not None:
        base["transcript_path"] = transcript_path

    if claude_kind == "SessionStart" or claude_kind == "Stop":
        return base

    tool_input = {
        "codex_hook_event_name": codex_kind,
        "codex_payload": event,
    }
    base["tool_name"] = "Edit"
    base["tool_input"] = tool_input
    if claude_kind == "PostToolUse":
        base["tool_response"] = {"codex_payload": event}
    return base


def to_claude_kind(kind: str) -> str:
    compact = "".join(ch for ch in kind.lower() if ch.isalnum())
    if compact in {"sessionstart", "startup", "resume", "clear"}:
        return "SessionStart"
    if compact in {"stop", "stopped"}:
        return "Stop"
    if compact in {"posttooluse", "postcompact", "subagentstop"}:
        return "PostToolUse"
    if compact in {"pretooluse", "permissionrequest", "precompact", "userpromptsubmit", "subagentstart"}:
        return "PreToolUse"
    return "PreToolUse"


def claude_hook_event_name(event: dict[str, Any], fallback: str) -> str:
    kind = str(
        event.get("hook_event_name")
        or event.get("event_name")
        or event.get("eventName")
        or event.get("event")
        or event.get("type")
        or ""
    )
    compact = "".join(ch for ch in kind.lower() if ch.isalnum())
    mapping = {
        "sessionstart": "SessionStart",
        "userpromptsubmit": "UserPromptSubmit",
        "userpromptexpansion": "UserPromptExpansion",
        "pretooluse": "PreToolUse",
        "posttooluse": "PostToolUse",
        "posttoolusefailure": "PostToolUseFailure",
        "posttoolbatch": "PostToolBatch",
        "precompact": "PreCompact",
        "postcompact": "PostCompact",
        "subagentstart": "SubagentStart",
        "subagentstop": "SubagentStop",
        "stop": "Stop",
        "stopfailure": "StopFailure",
        "sessionend": "SessionEnd",
    }
    return mapping.get(compact, fallback)


def compact_kind(event: dict[str, Any]) -> str:
    kind = str(
        event.get("hook_event_name")
        or event.get("event_name")
        or event.get("eventName")
        or event.get("event")
        or event.get("type")
        or ""
    )
    return "".join(ch for ch in kind.lower() if ch.isalnum())


def session_id(event: dict[str, Any]) -> str:
    return str(
        event.get("session_id")
        or event.get("thread_id")
        or event.get("threadId")
        or event.get("conversation_id")
        or event.get("conversationId")
        or "codex-session"
    )


def extract_prompt(event: dict[str, Any]) -> str:
    candidates = [
        event.get("prompt"),
        event.get("user_prompt"),
        event.get("userPrompt"),
        event.get("input"),
        event.get("message"),
    ]
    for candidate in candidates:
        if isinstance(candidate, str):
            return candidate
        if isinstance(candidate, list):
            return " ".join(str(item) for item in candidate)
    return json.dumps(event, ensure_ascii=False, separators=(",", ":"))[:4000]


def extract_tool_name(event: dict[str, Any]) -> str:
    for key in ("tool_name", "toolName", "name", "command"):
        value = event.get(key)
        if isinstance(value, str) and value:
            return value
    tool = event.get("tool")
    if isinstance(tool, dict):
        value = tool.get("name") or tool.get("tool_name")
        if isinstance(value, str):
            return value
    return "tool"


def summarize_tool_event(event: dict[str, Any]) -> str:
    parts = [f"tool: {extract_tool_name(event)}"]
    paths = extract_paths(event)
    if paths:
        parts.append("paths: " + ", ".join(paths[:6]))
    for key in ("error", "stderr", "output", "result", "tool_response", "toolResponse"):
        value = event.get(key)
        if isinstance(value, str) and value.strip():
            parts.append(f"{key}: {value[:3000]}")
        elif isinstance(value, (dict, list)):
            parts.append(f"{key}: {json.dumps(value, ensure_ascii=False, separators=(',', ':'))[:3000]}")
    if len(parts) == 1:
        parts.append(json.dumps(event, ensure_ascii=False, separators=(",", ":"))[:3000])
    return "\n".join(parts)


def extract_paths(value: Any) -> list[str]:
    paths: list[str] = []

    def visit(node: Any) -> None:
        if len(paths) >= 12:
            return
        if isinstance(node, dict):
            for key, child in node.items():
                lowered = str(key).lower()
                if lowered in {"path", "filepath", "file_path", "cwd"} and isinstance(child, str):
                    paths.append(child)
                else:
                    visit(child)
        elif isinstance(node, list):
            for item in node[:20]:
                visit(item)

    visit(value)
    deduped: list[str] = []
    for path in paths:
        if path not in deduped:
            deduped.append(path)
    return deduped


def extract_feedback(event: dict[str, Any]) -> str:
    for key in ("assistant_response", "assistantResponse", "response", "summary", "message"):
        value = event.get(key)
        if isinstance(value, str) and value:
            return value[:3000]
    return json.dumps(event, ensure_ascii=False, separators=(",", ":"))[:3000]


def env_disabled(*names: str) -> bool:
    for name in names:
        value = os.environ.get(name)
        if value is not None:
            return value in {"0", "false", "False"}
    return False


def env_enabled(name: str, default: bool = False) -> bool:
    value = os.environ.get(name)
    if value is None:
        return default
    return value not in {"0", "false", "False"}


def env_int_any(*names: str) -> int | None:
    for name in names:
        value = os.environ.get(name)
        if value is None:
            continue
        try:
            return int(value)
        except ValueError:
            continue
    return None


def env_float_any(*names: str) -> float | None:
    for name in names:
        value = os.environ.get(name)
        if value is None:
            continue
        try:
            return float(value)
        except ValueError:
            continue
    return None


def hook_scope() -> str | None:
    """The CALLER's scope, forwarded on every contextd request.

    Without this, a long-lived contextd applies its own inherited
    LUNARIS_HOOK_SCOPE to every request — cross-project scope bleed
    (216 episodes landed in another project's partition, 2026-07-14).
    Empty is treated as unset so Scope validation never sees "".
    """
    return os.environ.get("LUNARIS_HOOK_SCOPE") or None


def strip_none(value: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in value.items() if v is not None}


def stable_event_id(session_id: str, kind: str, event: dict[str, Any]) -> str:
    body = json.dumps(event, sort_keys=True, separators=(",", ":"), default=str)
    digest = hashlib.blake2s(f"{session_id}\0{kind}\0{body}".encode("utf-8"), digest_size=16)
    return "codex-" + digest.hexdigest()


if __name__ == "__main__":
    raise SystemExit(main())

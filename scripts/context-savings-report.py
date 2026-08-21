#!/usr/bin/env python3
"""ADD task `context-savings-telemetry` (contract FROZEN @ v1, engram-soul-loop
task 10) — per-scope context-savings aggregation report.

Makes Lunaris's window-context savings MEASURABLE, not argued: scans a
scope's `lunaris:{scope}:episode:*` keys over Moon, filters the
`lunaris:memory_injection` / `lunaris:turn_feedback` captures
(`crates/lunaris-hook/src/context.rs::trace_injection` /
`capture_feedback`), and prints the aggregation — injected tokens, turn
count, tool-call count, cited/uncited verdict counts + cited rate.

READ-ONLY: this module issues only SCAN and MGET/GET against Moon (plus the
PING connectivity probe) — never a write command. See
`scripts/tests/test_context_savings_report.py::ModuleIsReadOnly` for the
structural guard.

`aggregate(episodes: list[dict]) -> dict` is the pure core (no Moon
involved), tested directly against synthetic episode dicts. `main()` is the
thin Moon-reading CLI wrapper: raw RESP over a TCP socket, stdlib only —
zero third-party dependencies (mirrors the pattern in
`scripts/spike-scope-hashtag-probe.py`).

Usage:
  python3 scripts/context-savings-report.py --scope acme.agent-1
  python3 scripts/context-savings-report.py --scope acme.agent-1 --json
  python3 scripts/context-savings-report.py --scope acme.agent-1 \
      --host 127.0.0.1 --port 6381
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import sys
from typing import Any
from urllib.parse import urlparse

INJECTION_SOURCE = "lunaris:memory_injection"
FEEDBACK_SOURCE = "lunaris:turn_feedback"


def default_moon_endpoint() -> tuple[str, int]:
    """Default --host/--port from `LUNARIS_MOON_URL` (the convention every
    sibling Moon script honors, e.g. `moon://127.0.0.1:6380`), falling back to
    127.0.0.1:6380. Read at call time, never cached (issue #49 convention).
    NOTE: on boxes running the launchd Moon agent the live port is 6381 —
    6380 may be a foreign Redis; pass --port or set the env accordingly."""
    url = os.environ.get("LUNARIS_MOON_URL")
    if url:
        parsed = urlparse(url)
        if parsed.hostname:
            return parsed.hostname, parsed.port or 6380
    return "127.0.0.1", 6380

# Batch size for MGET calls against the scanned key list — keeps any single
# RESP command reasonably sized without adding a real limit on scope size.
MGET_BATCH = 200


# --------------------------------------------------------------------------
# Pure aggregation core (§3 CONTRACT) — no Moon, no I/O, fully unit-testable.
# --------------------------------------------------------------------------


def aggregate(episodes: list[Any]) -> dict[str, Any]:
    """Aggregate parsed episode docs into one per-scope savings report.

    `episodes` is the raw list of parsed JSON docs from the scope's episode
    keys (the `{"source": ..., "metadata": {...}}` shape every `lunaris:*`
    capture writes). A malformed entry (not a dict, missing/non-dict
    `metadata`) is skipped rather than raising — this function must never
    fail because of one bad row.
    """
    injected_tokens_total = 0
    injection_count = 0
    turns = 0
    tool_calls = 0
    cited = 0
    uncited = 0

    for episode in episodes:
        if not isinstance(episode, dict):
            continue
        metadata = episode.get("metadata")
        if not isinstance(metadata, dict):
            continue
        source = episode.get("source")

        if source == INJECTION_SOURCE:
            tokens = metadata.get("injected_tokens_est")
            if isinstance(tokens, (int, float)) and not isinstance(tokens, bool):
                injected_tokens_total += int(tokens)
                injection_count += 1
        elif source == FEEDBACK_SOURCE:
            turns += 1
            stats = metadata.get("transcript_stats")
            if isinstance(stats, dict):
                tool_call_count = stats.get("tool_call_count")
                if isinstance(tool_call_count, (int, float)) and not isinstance(
                    tool_call_count, bool
                ):
                    tool_calls += int(tool_call_count)
            verdicts = metadata.get("verdicts")
            if isinstance(verdicts, list):
                for verdict_row in verdicts:
                    if not isinstance(verdict_row, dict):
                        continue
                    verdict = verdict_row.get("verdict")
                    if verdict == "cited":
                        cited += 1
                    elif verdict == "uncited":
                        uncited += 1

    total_verdicts = cited + uncited
    cited_rate = (cited / total_verdicts) if total_verdicts > 0 else None

    return {
        "injected_tokens_total": injected_tokens_total,
        "injection_count": injection_count,
        "turns": turns,
        "tool_calls": tool_calls,
        "cited": cited,
        "uncited": uncited,
        "cited_rate": cited_rate,
    }


# --------------------------------------------------------------------------
# Raw RESP layer — stdlib socket only, read commands only.
# --------------------------------------------------------------------------


def _encode(*args: str) -> bytes:
    out = [f"*{len(args)}\r\n".encode()]
    for a in args:
        b = a.encode()
        out.append(b"$%d\r\n%s\r\n" % (len(b), b))
    return b"".join(out)


def _read_line(sock: socket.socket, buf: bytearray) -> bytes:
    while b"\r\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            raise ConnectionError("moon closed the connection")
        buf.extend(chunk)
    line, _, rest = bytes(buf).partition(b"\r\n")
    del buf[: len(line) + 2]
    return line


def _read_exact(sock: socket.socket, buf: bytearray, n: int) -> bytes:
    while len(buf) < n:
        chunk = sock.recv(4096)
        if not chunk:
            raise ConnectionError("moon closed the connection")
        buf.extend(chunk)
    out = bytes(buf[:n])
    del buf[:n]
    return out


def _read_reply(sock: socket.socket, buf: bytearray) -> Any:
    line = _read_line(sock, buf)
    if not line:
        raise ConnectionError("empty RESP reply line")
    kind, rest = line[:1], line[1:]
    if kind == b"+":
        return rest
    if kind == b"-":
        raise RuntimeError(rest.decode(errors="replace"))
    if kind == b":":
        return int(rest)
    if kind == b"$":
        n = int(rest)
        if n == -1:
            return None
        payload = _read_exact(sock, buf, n)
        _read_exact(sock, buf, 2)  # trailing CRLF
        return payload
    if kind == b"*":
        n = int(rest)
        if n == -1:
            return None
        return [_read_reply(sock, buf) for _ in range(n)]
    raise RuntimeError(f"unexpected RESP type byte: {line!r}")


class MoonConn:
    """Minimal RESP client — read commands only (SCAN / GET / MGET / PING)."""

    def __init__(self, host: str, port: int, timeout: float = 5.0):
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.buf = bytearray()

    def cmd(self, *args: str) -> Any:
        self.sock.sendall(_encode(*args))
        return _read_reply(self.sock, self.buf)

    def close(self) -> None:
        self.sock.close()


def _decode(value: Any) -> str | None:
    if value is None:
        return None
    if isinstance(value, bytes):
        return value.decode(errors="replace")
    return str(value)


def scan_keys(conn: MoonConn, match_pattern: str, count: int = 500) -> list[str]:
    """SCAN the full keyspace for `match_pattern`, following the cursor to
    completion. Read-only (SCAN never mutates)."""
    cursor = "0"
    keys: list[str] = []
    while True:
        reply = conn.cmd("SCAN", cursor, "MATCH", match_pattern, "COUNT", str(count))
        cursor = _decode(reply[0]) or "0"
        batch = reply[1] or []
        keys.extend(k for k in (_decode(item) for item in batch) if k is not None)
        if cursor == "0":
            break
    return keys


def fetch_episodes(conn: MoonConn, keys: list[str]) -> list[Any]:
    """MGET the episode keys in batches and JSON-decode each value. A
    missing key (None) or an undecodable value is skipped, not fatal."""
    episodes: list[Any] = []
    for i in range(0, len(keys), MGET_BATCH):
        batch = keys[i : i + MGET_BATCH]
        if not batch:
            continue
        values = conn.cmd("MGET", *batch)
        for raw in values or []:
            text = _decode(raw)
            if text is None:
                continue
            try:
                episodes.append(json.loads(text))
            except json.JSONDecodeError:
                continue
    return episodes


def collect_scope_episodes(host: str, port: int, scope: str) -> list[Any]:
    conn = MoonConn(host, port)
    try:
        pattern = f"lunaris:{scope}:episode:*"
        keys = scan_keys(conn, pattern)
        return fetch_episodes(conn, keys)
    finally:
        conn.close()


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------


def _format_report(scope: str, result: dict[str, Any]) -> str:
    cited_rate = result["cited_rate"]
    cited_rate_str = "n/a" if cited_rate is None else f"{cited_rate * 100:.1f}%"
    rows = [
        ("scope", scope),
        ("injected_tokens_total", str(result["injected_tokens_total"])),
        ("injection_count", str(result["injection_count"])),
        ("turns", str(result["turns"])),
        ("tool_calls", str(result["tool_calls"])),
        ("cited", str(result["cited"])),
        ("uncited", str(result["uncited"])),
        ("cited_rate", cited_rate_str),
    ]
    width = max(len(label) for label, _ in rows)
    return "\n".join(f"{label:<{width}}  {value}" for label, value in rows)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Per-scope context-savings aggregation (read-only over Moon)."
    )
    default_host, default_port = default_moon_endpoint()
    parser.add_argument("--scope", required=True, help="Lunaris scope to aggregate")
    parser.add_argument(
        "--host",
        default=default_host,
        help="Moon host (default: LUNARIS_MOON_URL host, else 127.0.0.1)",
    )
    parser.add_argument(
        "--port",
        type=int,
        default=default_port,
        help="Moon port (default: LUNARIS_MOON_URL port, else 6380)",
    )
    parser.add_argument("--json", action="store_true", help="print the result as JSON")
    args = parser.parse_args(argv)

    episodes = collect_scope_episodes(args.host, args.port, args.scope)
    result = aggregate(episodes)

    if args.json:
        print(json.dumps(result, indent=2))
    else:
        print(_format_report(args.scope, result))
    return 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""ADD task scope-hashtag-spike — multi-shard TXN probe (frozen @ v1, 2026-06-11).

Reproduces the evidence behind docs/design/scope-hashtag-txn-rfc.md:

  P1  --shards 4, unbraced 8-key TXN     -> PARTIAL/FULL rejection (>=1
      ERR_TXN_CROSS_SHARD; any OKs are keys that hashed to the accept shard)
  P2  --shards 4, fully {tag}-braced TXN -> HOMOGENEOUS per connection
      (8/8 OK iff the connection landed on the tag's shard, else 8/8 reject)
      and across N fresh connections at least one sees 8/8 reject — proving
      client-side hash-tagging alone CANNOT restore TXN atomicity, because
      SO_REUSEPORT picks the accept shard, not the client.
  P3  --shards 1 control                  -> 8/8 OK + clean TXN.COMMIT
      (the gap is latent on every current Lunaris deployment recipe).

Exit 0 iff all three observations match; non-zero on any deviation.

The script launches its own Moon instances (one 4-shard, one 1-shard) from
--moon-bin in a temp dir, so it needs no pre-running server. Raw RESP over a
TCP socket — zero Python dependencies.

Usage:
    python3 scripts/spike-scope-hashtag-probe.py [--port 6395] \
        [--moon-bin vendor/moon/target/release/moon] [--connections 16]
"""

from __future__ import annotations

import argparse
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

CROSS_SHARD_MARKER = b"cross-shard writes"


# ---------------------------------------------------------------- RESP layer


def encode(*args: str) -> bytes:
    out = [f"*{len(args)}\r\n".encode()]
    for a in args:
        b = a.encode()
        out.append(b"$%d\r\n%s\r\n" % (len(b), b))
    return b"".join(out)


def read_reply(sock: socket.socket, buf: bytearray) -> bytes:
    """Read exactly one simple/error/bulk/integer reply line (no arrays needed
    for SET/TXN.* replies). Returns the raw first line including type byte."""
    while b"\r\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            raise ConnectionError("server closed connection")
        buf.extend(chunk)
    line, _, _ = bytes(buf).partition(b"\r\n")
    del buf[: len(line) + 2]
    if line.startswith(b"$") and not line.startswith(b"$-1"):
        # bulk string: consume the payload too
        n = int(line[1:])
        while len(buf) < n + 2:
            chunk = sock.recv(4096)
            if not chunk:
                raise ConnectionError("server closed connection")
            buf.extend(chunk)
        payload = bytes(buf[:n])
        del buf[: n + 2]
        return b"$" + payload
    return line


class Conn:
    def __init__(self, port: int):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.buf = bytearray()

    def cmd(self, *args: str) -> bytes:
        self.sock.sendall(encode(*args))
        return read_reply(self.sock, self.buf)

    def close(self) -> None:
        self.sock.close()


# ------------------------------------------------------------- Moon launcher


def launch_moon(moon_bin: Path, port: int, shards: int, workdir: Path) -> subprocess.Popen:
    proc = subprocess.Popen(
        [str(moon_bin), "--port", str(port), "--shards", str(shards), "--dir", str(workdir)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        try:
            c = Conn(port)
            if c.cmd("PING") == b"+PONG":
                c.close()
                return proc
            c.close()
        except OSError:
            time.sleep(0.2)
    proc.kill()
    raise RuntimeError(f"moon --port {port} --shards {shards} did not come up in 15s")


# ------------------------------------------------------------------- probes


def txn_set_round(conn: Conn, keys: list[str]) -> tuple[int, int]:
    """TXN.BEGIN, SET each key, TXN.ABORT. Returns (ok, rejected)."""
    begin = conn.cmd("TXN", "BEGIN")
    if begin != b"+OK":
        raise RuntimeError(f"TXN BEGIN failed: {begin!r}")
    ok = rejected = 0
    for k in keys:
        reply = conn.cmd("SET", k, "v")
        if reply == b"+OK":
            ok += 1
        elif CROSS_SHARD_MARKER in reply:
            rejected += 1
        else:
            raise RuntimeError(f"unexpected SET reply for {k}: {reply!r}")
    conn.cmd("TXN", "ABORT")
    return ok, rejected


def probe_unbraced(port: int) -> tuple[int, int]:
    keys = [f"lunaris:acme.a1:fact:01HUNBRACED{i:04d}" for i in range(8)]
    conn = Conn(port)
    try:
        return txn_set_round(conn, keys)
    finally:
        conn.close()


def probe_braced(port: int, n_connections: int) -> list[tuple[int, int]]:
    keys = [f"lunaris:{{acme.a1}}:fact:01HBRACED{i:04d}" for i in range(8)]
    results = []
    for _ in range(n_connections):
        conn = Conn(port)
        try:
            results.append(txn_set_round(conn, keys))
        finally:
            conn.close()
    return results


def probe_single_shard(port: int) -> tuple[int, int, bytes]:
    keys = [f"lunaris:acme.a1:fact:01HCONTROL{i:04d}" for i in range(8)]
    conn = Conn(port)
    try:
        begin = conn.cmd("TXN", "BEGIN")
        if begin != b"+OK":
            raise RuntimeError(f"TXN BEGIN failed: {begin!r}")
        ok = rejected = 0
        for k in keys:
            reply = conn.cmd("SET", k, "v")
            if reply == b"+OK":
                ok += 1
            else:
                rejected += 1
        commit = conn.cmd("TXN", "COMMIT")
        return ok, rejected, commit
    finally:
        conn.close()


# --------------------------------------------------------------------- main


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--port", type=int, default=6395, help="base port (default 6395)")
    parser.add_argument(
        "--moon-bin",
        type=Path,
        default=Path("vendor/moon/target/release/moon"),
        help="moon binary (default vendor/moon/target/release/moon)",
    )
    parser.add_argument(
        "--connections",
        type=int,
        default=16,
        help="fresh connections for the braced probe (default 16; with 4 shards "
        "P[all land on the tag shard] = 0.25^N, so 16 makes a lucky-only run "
        "astronomically unlikely)",
    )
    args = parser.parse_args()

    if not args.moon_bin.exists():
        print(f"FAIL: moon binary not found at {args.moon_bin}", file=sys.stderr)
        return 2

    failures: list[str] = []
    rows: list[tuple[str, str, str]] = []  # (probe, observed, verdict)

    with tempfile.TemporaryDirectory(prefix="lunaris-shard-probe-") as tmp:
        tmp_path = Path(tmp)

        # ---- 4-shard server: P1 + P2
        moon4 = launch_moon(args.moon_bin, args.port, 4, tmp_path / "s4")
        try:
            ok, rej = probe_unbraced(args.port)
            obs = f"{ok} OK / {rej} cross-shard-rejected of 8"
            if rej >= 1 and ok + rej == 8:
                rows.append(("P1 unbraced TXN, shards=4", obs, "MATCH (partial/full reject)"))
            else:
                rows.append(("P1 unbraced TXN, shards=4", obs, "DEVIATION"))
                failures.append(f"P1: expected >=1 rejection out of 8, got {obs}")

            per_conn = probe_braced(args.port, args.connections)
            homogeneous = all(o == 8 or r == 8 for o, r in per_conn)
            n_all_ok = sum(1 for o, _ in per_conn if o == 8)
            n_all_rej = sum(1 for _, r in per_conn if r == 8)
            obs = (
                f"{len(per_conn)} conns: {n_all_ok} all-OK (lucky shard), "
                f"{n_all_rej} all-rejected, homogeneous={homogeneous}"
            )
            if homogeneous and n_all_rej >= 1:
                rows.append(("P2 {tag}-braced TXN, shards=4", obs, "MATCH (binding proven)"))
            else:
                rows.append(("P2 {tag}-braced TXN, shards=4", obs, "DEVIATION"))
                if not homogeneous:
                    failures.append(f"P2: per-connection results not homogeneous: {per_conn}")
                if n_all_rej == 0:
                    failures.append(
                        "P2: every connection landed on the tag's shard — cannot "
                        "demonstrate binding; rerun with more --connections "
                        "(or connection migration changed the story: update the RFC!)"
                    )
        finally:
            moon4.kill()
            moon4.wait()

        # ---- 1-shard control: P3
        moon1 = launch_moon(args.moon_bin, args.port + 1, 1, tmp_path / "s1")
        try:
            ok, rej, commit = probe_single_shard(args.port + 1)
            obs = f"{ok} OK / {rej} rejected of 8, TXN.COMMIT={commit.decode(errors='replace')}"
            if ok == 8 and rej == 0 and commit == b"+OK":
                rows.append(("P3 control, shards=1", obs, "MATCH (gap is latent)"))
            else:
                rows.append(("P3 control, shards=1", obs, "DEVIATION"))
                failures.append(f"P3: expected 8/8 OK + commit OK, got {obs}")
        finally:
            moon1.kill()
            moon1.wait()

    width = max(len(r[0]) for r in rows)
    print("\nVERDICT — multi-shard TXN probe (Moon @", args.moon_bin, ")")
    print("-" * 100)
    for probe, observed, verdict in rows:
        print(f"{probe:<{width}}  {verdict:<28}  {observed}")
    print("-" * 100)
    if failures:
        print("RESULT: DEVIATION — observation matrix does NOT hold:")
        for f in failures:
            print(f"  - {f}")
        return 1
    print(
        "RESULT: MATCH — TXN is shard-local AND binds to the accept shard; client-side\n"
        "hash-tagging alone cannot restore atomicity (see docs/design/scope-hashtag-txn-rfc.md)."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())

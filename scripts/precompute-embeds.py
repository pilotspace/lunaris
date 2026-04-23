"""Precompute + cache EmbeddingGemma vectors for a benchmark corpus.

Calls Ollama once per text and stores `sha256(text) -> float32[768]` in
a compact numpy `.npz` (safe, binary-efficient, no pickle). The
companion `ollama-replay-server.py` reads the same file and serves
cached vectors on the Ollama wire protocol so Lunaris can run without
ever hitting the real embedder.

Usage:
  uv run --with datasets --with requests --with numpy \
    python scripts/precompute-embeds.py \
      --out milestones/v0.1.1-bench/cache/squad-10k.npz \
      --squad-contexts 10000 --squad-questions 1000 \
      --chat-turns 10000 --chat-queries 1000
"""
from __future__ import annotations

import argparse
import hashlib
import os
import random
import time
from pathlib import Path

import numpy as np
import requests

OLLAMA = os.environ.get("OLLAMA_URL", "http://localhost:11434")
MODEL = os.environ.get("OLLAMA_MODEL", "embeddinggemma:300m")
DIM = 768


def sha(s: str) -> str:
    return hashlib.sha256(s.encode("utf-8")).hexdigest()


def embed(text: str) -> list[float]:
    r = requests.post(
        f"{OLLAMA}/api/embed",
        json={"model": MODEL, "input": [text]},
        timeout=30,
    )
    r.raise_for_status()
    return r.json()["embeddings"][0]


def collect_squad(n_docs: int, n_queries: int) -> tuple[list[str], list[str]]:
    from datasets import load_dataset

    ds = load_dataset("rajpurkar/squad", split="train")
    contexts: list[str] = []
    seen_ctx: set[str] = set()
    by_ctx_qs: dict[str, list[str]] = {}
    for row in ds:
        ctx = row["context"]
        if ctx not in seen_ctx:
            seen_ctx.add(ctx)
            contexts.append(ctx)
            by_ctx_qs[ctx] = []
        q = (row.get("question") or "").strip()
        if q:
            by_ctx_qs[ctx].append(q)
        if len(contexts) >= n_docs and all(len(by_ctx_qs[c]) >= 1 for c in contexts):
            break
    contexts = contexts[:n_docs]

    questions: list[str] = []
    i = 0
    cursor = [list(by_ctx_qs[c]) for c in contexts]
    while len(questions) < n_queries and any(cursor):
        idx = i % len(cursor)
        if cursor[idx]:
            questions.append(cursor[idx].pop())
        i += 1
    return contexts, questions


def collect_chat(
    n_turns: int, n_queries: int, prefix_chars: int
) -> tuple[list[str], list[str]]:
    from datasets import load_dataset

    ds = load_dataset("rajpurkar/squad", split="train")
    turns: list[str] = []
    seen: set[str] = set()
    for row in ds:
        q = (row.get("question") or "").strip()
        if len(q) >= 10 and q not in seen:
            seen.add(q)
            turns.append(q)
            if len(turns) >= n_turns:
                break
    turns = turns[:n_turns]
    rng = random.Random(7)
    query_idx = rng.sample(range(len(turns)), min(n_queries, len(turns)))
    queries = [turns[i][:prefix_chars] for i in query_idx]
    return turns, queries


def load_cache(path: Path) -> dict[str, np.ndarray]:
    if not path.exists():
        return {}
    with np.load(path, allow_pickle=False) as f:
        hashes = [h.decode() for h in f["hashes"]]
        vectors = f["vectors"]
    return {h: vectors[i] for i, h in enumerate(hashes)}


def save_cache(path: Path, cache: dict[str, np.ndarray]) -> None:
    if not cache:
        return
    hashes = list(cache.keys())
    vectors = np.stack([cache[h] for h in hashes]).astype(np.float32)
    np.savez(
        path,
        hashes=np.array([h.encode() for h in hashes]),
        vectors=vectors,
    )


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--squad-contexts", type=int, default=0)
    ap.add_argument("--squad-questions", type=int, default=0)
    ap.add_argument("--chat-turns", type=int, default=0)
    ap.add_argument("--chat-queries", type=int, default=0)
    ap.add_argument("--chat-prefix-chars", type=int, default=80)
    ap.add_argument(
        "--extend",
        action="store_true",
        help="Load existing cache and only embed missing texts",
    )
    args = ap.parse_args()

    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)

    cache: dict[str, np.ndarray] = {}
    if args.extend:
        cache = load_cache(out)
        print(f"[resume] loaded {len(cache)} existing entries from {out}")

    texts: list[str] = []
    if args.squad_contexts or args.squad_questions:
        ctxs, qs = collect_squad(
            max(args.squad_contexts, 0), max(args.squad_questions, 0)
        )
        texts.extend(ctxs)
        texts.extend(qs)
        print(f"[collect] SQuAD: {len(ctxs)} contexts + {len(qs)} questions")
    if args.chat_turns or args.chat_queries:
        turns, queries = collect_chat(
            max(args.chat_turns, 0),
            max(args.chat_queries, 0),
            args.chat_prefix_chars,
        )
        texts.extend(turns)
        texts.extend(queries)
        print(f"[collect] chat: {len(turns)} turns + {len(queries)} query prefixes")

    dedup: list[str] = []
    seen: set[str] = set()
    for t in texts:
        if t not in seen:
            seen.add(t)
            dedup.append(t)
    print(f"[collect] {len(dedup)} unique texts to (re)embed")

    t0 = time.perf_counter()
    new = 0
    for i, text in enumerate(dedup, start=1):
        h = sha(text)
        if h in cache:
            continue
        vec = np.array(embed(text), dtype=np.float32)
        if vec.shape != (DIM,):
            raise RuntimeError(f"expected {DIM}-d vector, got shape {vec.shape}")
        cache[h] = vec
        new += 1
        if new % 250 == 0:
            elapsed = time.perf_counter() - t0
            rate = new / elapsed
            remaining = (len(dedup) - i) / rate if rate else 0.0
            print(
                f"  [{new} new / {i}/{len(dedup)}]  "
                f"elapsed {elapsed:.0f}s  rate {rate:.1f}/s  ETA {remaining:.0f}s",
                flush=True,
            )

    save_cache(out, cache)
    size_mb = out.stat().st_size / (1024 * 1024)
    print(
        f"[done] {len(cache)} entries ({new} new) written to {out} "
        f"({size_mb:.1f} MB)"
    )


if __name__ == "__main__":
    main()

# LongMemEval (LME) benchmark harness

The runner behind Lunaris's headline recall-quality number, and the A/B that
gates the 0.7.0 graph decision. It drives the `lunaris-evals` binary
(`crates/lunaris-bench/src/bin/evals.rs`) one question per process against a
throwaway Moon, then scores the results.

Ported into version control in 0.6.2. Until then every runner lived only in
the operator's gitignored `tmp/` directory — the numbers gating a release
could not be reproduced from a clone by anyone else.

---

## Read this before you run anything

### 1. The bench Moon is destroyed between every question

LongMemEval-S requires each question to retrieve from **its own** haystack.
Moon's `StartsWith` source filter fails open, so physical isolation — a
`FLUSHALL` between questions — is the only sound boundary. Every runner here
flushes its target Moon on every attempt.

**Port 6381 is the operator's live personal memory store** (hundreds of
thousands of keys, launchd unit `dev.lunaris.moon-6381`). Aiming this harness
at it destroys it. `lib.sh` hard-refuses 6381 in `lme_guard_port` /
`lme_guard_url`, and every entry point calls the guard before it does anything
destructive — including in `--dry-run`. The refusal exits `4`:

```
$ MOON_PORT=6381 scripts/bench/lme/run_lme.sh --dry-run
FATAL: MOON_PORT resolved to port 6381, which is RESERVED.
  ...
$ echo $?
4
```

Add more reserved ports with `LME_RESERVED_PORTS="7000 7001"`.

The default bench port is **6399**. Nothing you care about should be there.

### 2. One process per question is mandatory

`LUNARIS_EVAL_LME_LIMIT=1` plus a fresh process per offset is load-bearing for
two independent reasons:

* **Correctness** — per-question haystack isolation (above).
* **Stability** — batching questions into one long-lived process leaks Metal
  buffers and eventually wedges the run. The in-process llama.cpp embedder
  also deadlocks under GPU contention, which is why the default embedder lane
  points at a warm Ollama server instead (`--features embed-remote`).

Do not "optimise" this into a single process. It has been tried; it fails
slowly and silently.

### 3. The judge/generation noise floor is about ±5 points

Proven 2026-07-30: the same config re-run produced **byte-identical retrieval
on 108/108 questions and still flipped 10 verdicts**. Temperature is already
0 on both the generator and the judge; the remaining variance is the provider's.

Consequences:

* A sub-5-point delta is **not signal**. Do not chase it, do not ship on it,
  do not write it in a changelog as an improvement.
* The control for an arm is **the other arm of the same run**, never a number
  from a previous day. That is why `ab_run.sh` runs graph-off and graph-on
  back to back over the same offsets.
* A partial arm is not comparable to a complete one. `tally.py` prints
  `RUN NOT FINAL` until coverage is complete and `ERR == 0`.

### 4. ERR is not a wrong answer

A judge failure, a watchdog kill, or a truncated log is **ERR**, and ERR is
excluded from the ratio rather than counted as a miss. Counting judge outages
as wrong answers is how a 20-question provider outage once looked like a
5-point regression.

---

## Prerequisites

| What | How |
| --- | --- |
| Rust toolchain | workspace MSRV (see `rust-toolchain.toml`) |
| `moon` binary | `$HOME/.lunaris/bin/moon`, or set `LME_MOON_BIN` |
| `redis-cli` | any recent redis client; used for PING/FLUSHALL |
| `python3` | 3.11+, stdlib only (`tally.py`) |
| `perl` | preinstalled on macOS/Linux; provides the `alarm(2)` watchdog because neither `timeout` nor `gtimeout` exists on the reference box |
| Ollama | serving the embedding model, default `http://127.0.0.1:11434` |
| Reranker GGUF | `bge-reranker-v2-m3.Q5_K_M.gguf` under `~/.lunaris/models/`; stage with `cargo run -p lunaris-bench --bin stage-models`, or point `LUNARIS_RERANKER_GGUF` at your own copy |
| MiniMax API key | generation + judging + graph-arm extraction |
| LongMemEval-S dataset | downloaded on first use into `LUNARIS_EVAL_CACHE_DIR`; **not committed** (it is large and externally licensed) |

Build the eval binary:

```bash
cargo build --release -p lunaris-bench --bin lunaris-evals \
  --features embed-remote,llamacpp,metal      # macOS / Apple silicon
# Linux + NVIDIA:  --features embed-remote,llamacpp,cuda
# CPU only:        --features embed-remote,llamacpp
```

`embed-remote` is not optional for the default lane. Without it,
`Lunaris::open()` silently resolves `NoopEmbedder`, every vector is zeros, and
the run **still prints a J-score** computed over BM25 plus insertion-order
tie-breaks. `crates/lunaris-core/tests/sdk_feature_forwarding.rs::bench_embed_remote_forwards_to_umbrella`
pins the forward so the feature cannot silently disappear again.

---

## Environment

Nothing personal is baked into these scripts. Everything below has a safe
default; only the API key is mandatory.

| Variable | Default | Meaning |
| --- | --- | --- |
| `MINIMAX_API_KEY` | *(none — required)* | Provider key. Never logged, never written to an artifact. |
| `LUNARIS_BENCH_KEY_FILE` | *(unset)* | Alternative: path to a file holding the key. Keep it **outside the repo**. |
| `MOON_PORT` | `6399` | Bench Moon port. `6381` is refused. |
| `MOON_URL` | `moon://127.0.0.1:$MOON_PORT` | Full override; also guarded. |
| `LME_RESERVED_PORTS` | *(empty; 6381 always added)* | Extra ports to refuse. |
| `ARM` | `graphoff` | `graphoff` \| `graphon` (`LUNARIS_EVAL_LME_GRAPH` 0/1). |
| `OFFSETS_FILE` | `questions/offsets125.tsv` | Question-set manifest. |
| `LME_EVAL_BIN` | `target/release/lunaris-evals` | Eval binary. |
| `LME_MOON_BIN` | `$HOME/.lunaris/bin/moon` | Moon server binary. |
| `LME_RESULTS_DIR` | `target/lme` | Artifacts. Under `target/`, so gitignored. |
| `LME_EXTRACT_CACHE_DIR` | `$LME_RESULTS_DIR/extract-cache` | Extraction cache. |
| `LUNARIS_EVAL_CACHE_DIR` | `~/.cache/lunaris/eval-hub` | Dataset cache. |
| `OLLAMA_URL` | `http://127.0.0.1:11434` | Embedder endpoint. |
| `EMBED_MODEL` | `granite-embed-r2` | Ollama model tag for the embedder. |
| `LUNARIS_RERANKER_GGUF` | `~/.lunaris/models/bge-reranker-v2-m3.Q5_K_M.gguf` | Reranker weights. |
| `GEN_MODEL` / `JUDGE_MODEL` | `minimax-m3:cloud` | Answer generator / judge. |
| `QTIMEOUT` | `1500` (fill: `1800`) | Per-question watchdog, seconds. |
| `MAX_ATTEMPTS` | `3` | Retries per question. |
| `SHARDS` | `5` | Parallel fill shards. |
| `SHARD_PORT_BASE` | `6410` | First fill-shard Moon port. |

---

## Files

| File | Role |
| --- | --- |
| `lib.sh` | Path resolution, reserved-port guards, key loading, Moon helpers, watchdog. Sourced by every entry point. |
| `run_lme.sh` | Measured runner for **one** arm. Resume-safe, fingerprinted. |
| `fill_cache.sh` | Parallel extraction-cache fill (no judge, no rerank, no llama.cpp). |
| `ab_run.sh` | graph-off then graph-on over the same offsets. |
| `chain_fill_then_ab.sh` | fill → coverage check → A/B, with the Moon watchdog running. The single command for a full run. |
| `moon_watchdog.sh` | Restarts the bench Moon if it dies mid-run. |
| `tally.py` | Three-way scoring (correct / wrong / ERR) + FINAL determination. |
| `questions/offsets125.tsv` | The canonical N=125 stratified manifest, with categories. |
| `questions/offsets16.tsv` | 16-question shakeout subset. Never report deltas from it. |
| `questions/offsets_smoke.tsv` | One question. Plumbing check only. |

---

## Running a full N=125 A/B from a fresh clone

```bash
# 0. Verify config without executing anything. Do this first, every time.
scripts/bench/lme/chain_fill_then_ab.sh --dry-run

# 1. Bench Moon on 6399 (the chain starts a watchdog too, but having it up
#    front makes the dry-run preflight informative).
scripts/bench/lme/moon_watchdog.sh &

# 2. Credentials — from a file outside the repo, or straight in the env.
export LUNARIS_BENCH_KEY_FILE=~/.config/lunaris/minimax.key

# 3. Warm the embedder.
ollama serve &            # then ensure the granite-embed-r2 tag is present

# 4. Go. This is long; detach it.
nohup scripts/bench/lme/chain_fill_then_ab.sh > /dev/null 2>&1 & disown
tail -f target/lme/chain.log
```

Scoring, at any point:

```bash
python3 scripts/bench/lme/tally.py --dir target/lme/graphoff --expected 125
python3 scripts/bench/lme/tally.py --dir target/lme/graphon  --expected 125
```

### Expected wall clock (reference: M3 Max, warm Ollama, MiniMax cloud)

| Stage | Cold cache | Warm cache |
| --- | --- | --- |
| `fill_cache.sh`, 125 questions, `SHARDS=5` | 3–5 h | minutes (resumes) |
| `run_lme.sh` graph-off arm | ~2.5–3 h | same |
| `run_lme.sh` graph-on arm | ~2.5–3 h **with** a filled cache; **~40 h without it** | ~2.5–3 h |
| **Full A/B via `chain_fill_then_ab.sh`** | **~9–11 h** | ~5–6 h |

The graph arm's 20 min → ~75 s per question collapse comes entirely from the
extraction cache. Skipping the fill pass does not save time; it moves the
extraction cost into the measured run and multiplies it.

### Cache behaviour

* **Extraction cache** (`LME_EXTRACT_CACHE_DIR`) is content-addressed on chunk
  text plus model plus prompt template. Identical chunks replay from disk
  instead of re-calling the provider. It is shared across arms and runs by
  design: graph-off never extracts, and a prompt or model change produces new
  keys rather than stale hits. Writes are tmp+rename atomic, so parallel
  fillers never tear. Delete the directory to force a clean re-extraction.
* **Dataset cache** (`LUNARIS_EVAL_CACHE_DIR`) holds the downloaded
  LongMemEval-S corpus. Shared with every other eval; safe to keep.
* **Resume** is keyed off per-question artifacts, not the run log. A question
  counts as done only if its `.json` says `PASS|FAIL` **and** its `.log`
  carries an `LME_VERDICT` line with a `correct` key **and** the log has no
  `judge error`. A `SKIPPED` row or a truncated log never counts.
* **Fingerprint** — `run_lme.sh` writes `config.fp` (config env + binary
  SHA-256 + git HEAD) into the arm directory on first use and refuses to
  resume into it under a different config or binary. Exit code `3`. Use a
  fresh `DIR` instead of mixing runs.

---

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Completed (check the tally for `RUN FINAL`). |
| `2` | Bad configuration — unknown flag, missing key, unreadable offsets file, unbuilt binary. |
| `3` | Fingerprint mismatch: the artifact directory belongs to a different config/binary. |
| `4` | **Reserved-port refusal.** The target resolved to the live store. |

---

## What deliberately stayed in `tmp/`

The operator's `tmp/` tree holds ~40 one-off runners accumulated across the
candle era, VM experiments, and abandoned A/B variants. Only the current
generation is here. In particular, `tmp/llamacpp_gate/run_gate50.sh` and its
siblings **target port 6381 and FLUSHALL it** — they predate the guard and are
exactly what this port refuses to become. They stay in `tmp/` as historical
working copies; do not resurrect them.

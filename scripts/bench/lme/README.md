# LongMemEval (LME) benchmark harness

The runner behind Lunaris's LongMemEval recall-quality measurements, and the
A/B that gates the 0.7.0 graph decision. It drives the `lunaris-evals` binary
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

*(Canonical write-up, including the figure that was **retired** for having no
evidence behind it:
[`docs/benchmarks/measurement-noise.md`](../../../docs/benchmarks/measurement-noise.md).)*

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
| `LME_RESULTS_DIR` | `target/lme` | Artifacts. Under `target/`, so **gitignored** — see the warning below. |
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

### `target/lme` is gitignored — a run that ends there leaves no evidence

This is not a detail. It is the direct cause of the `85.4% (427/500)`
retraction: the headline's per-question artifacts were written under a
gitignored path, the working tree was cleaned, and the number became
undefendable ([`docs/benchmarks/v0.7-longmemeval-jscore-validation.md`](../../../docs/benchmarks/v0.7-longmemeval-jscore-validation.md)).

**Before any number from a run is published anywhere, commit its
envelope:**

```sh
python3 scripts/bench/publish_raw.py --benchmark lme \
  --dir target/lme/graphoff --expected 125 \
  --operating-point quality --arm graphoff \
  --out docs/benchmarks/lme-raw/$(date -u +%F)-n125-graphoff-quality.json
```

Conventions, schema and the empty-directory rationale:
[`docs/benchmarks/lme-raw/README.md`](../../../docs/benchmarks/lme-raw/README.md).

### Which operating point is this run?

`run_lme.sh` and `anygold_gate.sh` (default `RERANK=1`) measure the
**quality** operating point — rerank ON. The **shipped default is
`fast`** (rerank OFF). Say which one every number came from; the
publisher enforces it and refuses a mislabelled envelope.
See [`docs/benchmarks/operating-points.md`](../../../docs/benchmarks/operating-points.md).

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
| `tally.py` | Three-way scoring (correct / wrong / ERR) + FINAL determination. `--anygold` scores retrieval (evidence_recall_hit) instead of judge verdicts; `--baseline` / `--write-baseline` drive the CI ratchet. |
| `anygold_gate.sh` | Judge-free any-gold ratchet — the gate behind `.github/workflows/recall-ratchet.yml`. Starts and owns its own scratch Moon; needs no API key. |
| `baselines/ci-anygold.json` | The checked-in ratchet baseline (hits / total / tolerance / config signature / operating point). Re-bless with `anygold_gate.sh --write-baseline`. |
| `baselines/README.md` | **Which operating point the ratchet gates, why the N=16 gate could not fail, and the N=40 replacement.** Read before changing anything in `baselines/`. |
| `questions/offsets125.tsv` | The canonical N=125 stratified manifest, with categories. |
| `questions/offsets40.tsv` | N=40 CI-ratchet manifest — all six categories, deterministic derivation from `offsets125.tsv`. |
| `questions/offsets16.tsv` | 16-question shakeout subset — **2 of 6 categories only**. Never report deltas from it. |
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

## The CI recall ratchet (`anygold_gate.sh`)

`.github/workflows/recall-ratchet.yml` runs a **judge-free** slice of this
harness on every recall-affecting push to main: LongMemEval-S
evidence-recall **any-gold** over `questions/offsets16.tsv`, graph-OFF, one
process per question, against a scratch Moon the gate starts itself. Any-gold
(a gold-evidence session present in the capped reader context) needs no LLM
judge and no extraction provider, so the gate runs on a stock hosted runner
with only the embedder + reranker GGUFs and the public HF dataset — and it is
deterministic, so unlike J-score there is no ±5-point noise floor to hide in.

> **⚠ Two known defects in the shipped gate (found 2026-08-21, W3.7).**
> It measures the **quality** operating point (`rerank=1`) while the shipped
> default is **fast** (rerank OFF) — so the configuration users actually get
> is un-ratcheted. And at N=16 with tolerance 1 the fail floor is 14/16: the
> gate only trips on a **12.5-point** retrieval drop, and `offsets16.tsv`
> covers 2 of the 6 LongMemEval-S categories, so a regression in
> `temporal-reasoning` or `knowledge-update` is invisible at any N.
> The decision, the sensitivity arithmetic and the N=40 replacement are in
> [`baselines/README.md`](baselines/README.md). Do not read the current gate
> passing as evidence that recall quality is stable.

The result ratchets against `baselines/ci-anygold.json` with an explicit
per-question tolerance. The baseline records the retrieval-config signature
it was measured under; the gate refuses to compare across a config change
(exit 6). After an accepted, understood change:

```bash
cargo build --release -p lunaris-bench --bin lunaris-evals --features llamacpp
MOON_PORT=6455 LME_MOON_BIN=<moon-binary> \
  scripts/bench/lme/anygold_gate.sh --write-baseline scripts/bench/lme/baselines/ci-anygold.json
```

Two deliberate differences from `run_lme.sh`: the gate uses the IN-PROCESS
llama.cpp embedder (CI has no warm Ollama; on CPU, one process per question,
the Metal-contention deadlock lane does not exist), and it refuses to run
against any Moon it did not start — including the watchdog's 6399 — because
it flushes between questions.

CI wall clock: at ~2 min/question (M3 Max CPU; slower on hosted x86) a
single 16-question job would blow the budget, so the workflow shards the
manifest 4 ways (`SHARD_INDEX`/`SHARD_COUNT`, round-robin so category
stratification survives) and a fan-in job merges the shard artifacts before
the one baseline comparison. Sharded invocations refuse `--baseline` /
`--write-baseline` — a shard alone is a partial arm.

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

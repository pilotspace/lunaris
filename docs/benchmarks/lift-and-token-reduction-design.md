# Design — lift and token reduction as the headline measurement

**Status: DESIGN ONLY. Nothing here has been run. No number in this
document is a result.**

Owner decision W3.5 + W3.6 (`docs/planning/2026-08-21-ship-plan.md`):
lead with **lift on a fixed agent harness** and with **token reduction**;
keep absolute LongMemEval / PersonaMem scores as a secondary table for
readers comparing against published Zep and Mem0 figures. This document
specifies the measurement. Executing it is separate work.

---

## 1. Why lift, and what we are actually competing on

TencentDB-Agent-Memory publishes, per dataset: *baseline / with-plugin /
relative gain / baseline tokens / plugin tokens / token reduction*. It is
a good table. It is also the right shape for the question a buyer
actually asks — "what changes if I add this?" — which an absolute
LongMemEval score does not answer, because the reader model dominates it.

We adopt that shape and beat it on exactly one axis:

> **A stranger can re-run our table. They cannot re-run theirs.**

Their repository has 173 files, 9 test files, one CI workflow and no
benchmark harness. Ours ships the harness, the manifests, the config
fingerprints, and — as of this design — the raw envelope behind every
published cell. That is the whole pitch, and it survives only if we hold
ourselves to it first. Section 7 lists what "re-runnable" has to mean
concretely, and it is a checklist, not a slogan.

We are **not** claiming a head-to-head against Tencent, Zep or Mem0. Lift
is measured inside one harness with one reader held constant. Comparing
our lift to theirs compares two different experiments that happen to be
shaped alike. Say so in every table caption.

---

## 2. The harness

**Primary: the two benchmark harnesses already in this repository**, not
a new bespoke agent.

| | LongMemEval-S | PersonaMem 32k |
|---|---|---|
| Runner | `scripts/bench/lme/` | `scripts/bench/pm/` |
| Task shape | open-ended QA over ~50-session haystacks | 4-option MCQ over multi-session persona histories |
| Scoring | LLM judge | **exact letter match, no judge** |
| Noise floor | ±5 points ([`measurement-noise.md`](measurement-noise.md)) | no judge noise; hosted-reader risk only |
| Gold labels | yes | yes |
| Already committed | yes | yes |

Two datasets, not one, because they fail differently: LongMemEval is
where token reduction is dramatic (a full haystack is enormous) and where
the judge noise floor bites; PersonaMem is where scoring is deterministic
and a small lift is still readable.

### Why not a bespoke "real agent" harness

It was considered and rejected as the *primary*. A Claude Code / LangGraph
agent doing real tasks is closer to the pitch, but it has no gold labels,
no published comparison points, and — fatally for the one axis we are
competing on — a stranger cannot re-run it without our credentials, our
task set and our judgement about what "done" means. It is a good *demo*
and a bad *benchmark*. If one is built later it goes beside this table,
never instead of it.

---

## 3. Arms

Per dataset, per question. All arms answer the **same questions** with
the **same reader model** and the **same prompt template** — only the
context-assembly step differs.

| Arm | Context the reader sees | Operating point | Role |
|---|---|---|---|
| **A0 — no memory** | question (+ options) only | n/a | the floor; interprets everything else |
| **A1 — full context** | the entire prior conversation, truncated by a committed rule | n/a | **the baseline for lift and for token reduction** |
| **A2f — +Lunaris, fast** | production recall root, **rerank OFF** | `fast` | **the headline arm** — this is what ships |
| **A2q — +Lunaris, quality** | production recall root, **rerank ON** | `quality` | the "if you pay ~1.3 s" row |

**A1 is the baseline, not A0.** An agent without a memory system does not
answer from nothing; it stuffs the history into the context window. A0
answers "does memory help at all" (the +33.1-point PersonaMem number, and
Tencent's +28). A1 answers the question a buyer is really asking: "I am
already pasting the transcript — what does Lunaris buy me?" Publish both
lifts, clearly labelled; lead with A2f − A1.

**A1's truncation rule is load-bearing and must be committed, not
improvised.** When the history exceeds the reader's context window:
most-recent-first, whole messages only, never mid-message, and the number
of dropped messages recorded per question in the artifact. A silently
different truncation rule turns the baseline into whatever we want it to
be. The rule lives in the harness source and its identity goes into the
config signature.

**Both A2 arms run at the k the product ships**, not a tuned k. If a
different k is used, it is a different row with a different label.

---

## 4. Token accounting

**Provider-reported only.** Every arm records `usage.prompt_tokens` and
`usage.completion_tokens` from each chat response. If a provider does not
return usage for a call, that call's arm **does not publish a token
number** — a locally-estimated token count is a guess wearing a
measurement's clothes, and the whole point of this table is that ours are
not guesses.

Three quantities per arm, per question:

| Quantity | Definition |
|---|---|
| `read_prompt_tokens` | prompt tokens for the answer call — the context the reader consumed |
| `read_completion_tokens` | completion tokens for the answer call |
| `write_tokens_amortised` | **all** provider tokens spent building memory for this scope (extraction, distillation, verification), divided by the questions asked against that scope |

`write_tokens_amortised` is the column Tencent does not publish. A memory
system that spends 40k extraction tokens per session to save 20k
retrieval tokens is not saving anything, and a table that hides ingest
cost cannot distinguish the two. A0 and A1 have `write_tokens_amortised = 0`
by construction. Report the total as well as the split; if our total is
worse than A1's, publish that.

Token reduction, stated exactly:

```
read-token reduction  = 1 − (A2.read_prompt_tokens / A1.read_prompt_tokens)
total-token reduction = 1 − ((A2.read_prompt_tokens + A2.write_tokens_amortised)
                             / A1.read_prompt_tokens)
```

Report **median and p95** per question, never the mean alone: prompt-token
distributions across a haystack dataset are long-tailed and a mean flatters
whichever arm has the fewer huge questions.

### Harness gap this requires closing first

Nothing in `crates/lunaris-bench/src/eval/` records provider token usage
today (the only `usage` reference in `lme_judge.rs` does not capture it).
Capturing it is a prerequisite task, not part of the run: thread
`usage` off the chat response into the per-question verdict payload, for
both the generation call and — separately, never merged — the judge call.
**Judge tokens are instrument cost and must never appear in a published
token column.**

---

## 5. Runs, pairing and the error bar

**Pairing is mandatory.** Every arm answers the identical question set,
so the informative statistic is paired: **McNemar over the discordant
pairs**, not a difference of two independent proportions. Issue #141 is
the cautionary tale — a 10.3-point "gap" between unpaired populations
collapsed to two questions and p = 0.50 once the arms were matched.

**Arms are interleaved within a run, never run on different days.** The
control for an arm is the other arm of the same session
([`measurement-noise.md`](measurement-noise.md)).

| Dataset | N | Repeats | Reported band | Publish only if |
|---|---|---|---|---|
| LongMemEval-S | 125 (`questions/offsets125.tsv`) | **3** full interleaved repeats | mean across repeats, ± half the observed range, **plus** McNemar p on the pooled pairs | lift exceeds the ±5-point judge floor **and** McNemar p < 0.05 |
| PersonaMem 32k | 589 questions / 37 contexts | **2** interleaved repeats | mean ± half-range; McNemar p on paired questions | McNemar p < 0.05 |

Three repeats for LongMemEval is not conservatism: the judge floor is
±5 points and a single run cannot distinguish a 4-point lift from zero.
Two repeats for PersonaMem exist to catch a hosted reader changing under
us, not to average judge noise — there is no judge.

**`suggest_new_ideas` is excluded from the PersonaMem lift headline** and
reported separately as a known-degenerate category (issue #141). It is
kept in the *absolute* score's denominator, where it understates us, and
dropped from the *lift* figure, where it is noise on a length artifact.
State the exclusion in the caption; never do it silently.

---

## 6. The published tables

Two tables, in this order. Every row states its operating point.

### Table 1 (headline) — lift and token reduction

| Dataset | Reader | Baseline (A1, full context) | +Lunaris | Operating point | Lift | Read tokens: baseline → Lunaris | Read-token reduction | Amortised write tokens |
|---|---|---|---|---|---|---|---|---|
| LongMemEval-S | *(model)* | — | — | `fast` | — | — | — | — |
| LongMemEval-S | *(model)* | — | — | `quality` | — | — | — | — |
| PersonaMem 32k | *(model)* | — | — | `fast` | — | — | — | — |
| PersonaMem 32k | *(model)* | — | — | `quality` | — | — | — | — |

Every cell links to the committed envelope it came from. A cell with no
envelope does not get published — the dashes above are the honest state
until the run happens.

Add a second block, clearly separated, for lift **vs the no-memory floor
(A0)** — that is the comparison Tencent's +28 and our historical +33.1
belong to, and putting it in the same block as the A1 lift is how the two
get conflated.

### Table 2 (secondary) — absolute scores

Absolute LongMemEval / PersonaMem numbers with reader, judge, split,
operating point, N and error bar, for readers comparing against published
Zep / Mem0 figures — with the standing caveat that those are not
head-to-head ([`README.md`](README.md#competitor-figures)).

---

## 7. What "a stranger can re-run it" has to mean

Not aspirational. Each line is a check on the finished work.

1. **One command per arm**, with a `--dry-run` that validates the whole
   configuration and spends zero provider tokens.
2. **Public data only.** Both datasets download from Hugging Face without
   credentials. Nothing in the reproduction path reads `.planning/` or
   any private submodule.
3. **Config fingerprint** on every artifact directory; resuming into a
   directory measured under a different config or binary aborts (the
   existing `run_lme.sh` / `run_pm.sh` behaviour, exit 3).
4. **Committed manifests** — the exact question sets, not "a random 125".
5. **Committed prompt templates and model IDs**, versioned. A prompt edit
   is a measurement change.
6. **A committed envelope per arm** under [`lme-raw/`](lme-raw/README.md)
   / [`pm-raw/`](pm-raw/README.md), carrying per-question correct/wrong/ERR
   offsets and the token aggregates, produced by
   `scripts/bench/publish_raw.py` so the required fields cannot be
   omitted.
7. **A `lift.json`** per dataset combining the arms: the paired counts,
   the McNemar statistic, the token medians and p95s. Derived by a
   committed script from the committed envelopes — never typed by hand.
8. **A CI smoke job** running one question through every arm on each
   push that touches the harness, so it cannot bit-rot between the
   expensive runs. One question proves plumbing, gates nothing, and costs
   minutes.
9. **Secrets are the only thing the stranger must supply**, and the
   README says exactly which and where to get them.

---

## 8. Cost, wall time, and what blocks it

### Wall time

Derived from the figures recorded in
[`scripts/bench/lme/README.md`](../../scripts/bench/lme/README.md); the
PersonaMem runner has **no recorded wall clock**, which is itself a gap
to close on the first run.

| Stage | Estimate | Source |
|---|---|---|
| LME extraction-cache fill, 125 q, `SHARDS=5` | 3–5 h cold, minutes warm | recorded |
| One LME arm, 125 q, warm cache | ~2.5–3 h | recorded |
| **LME: 4 arms × 3 repeats** | **~30–36 h serial** | 12 × 2.5–3 h |
| PersonaMem, one arm, 32k split | **unmeasured** — record it | — |
| **PersonaMem: 4 arms × 2 repeats** | **unknown until measured** | — |

Two structural constraints stop this collapsing under parallelism:

- **One llama.cpp process machine-wide** — concurrent Metal contexts
  deadlock. The `quality` arm needs the in-process reranker, so `A2q`
  repeats cannot run in parallel with each other on one box. `A0`, `A1`
  and (with the remote-embedder lane) `A2f` can overlap.
- **Provider rate limits** on the generation and judge calls.

Realistic plan: **budget a week of mostly-unattended machine time** for
the LongMemEval side, run the PersonaMem side alongside it on the arms
that do not need llama.cpp, and treat the first repeat as a pilot whose
job is to produce the missing wall-clock numbers and catch harness gaps
before 30 hours are spent.

### Prerequisites, and who is blocked

| Prerequisite | Status |
|---|---|
| `MINIMAX_API_KEY` — LongMemEval generation, judging, and graph-arm extraction | **OWNER-BLOCKED.** Flag it; never handle or log it. |
| Reader credentials for the PersonaMem arms (`claude-sonnet-5`, `claude-opus-5` via the local Claude-CLI bridge) | **OWNER-BLOCKED** |
| Token-usage capture in the harness (§4) | engineering task — must land *before* the run, or the run produces no token table |
| A1 full-context arm + its committed truncation rule (§3) | engineering task — **does not exist today**; both harnesses currently have only A0 and A2 |
| `lift.json` derivation script + `publish_raw.py` token fields | engineering task |
| A dedicated bench Moon on 6399+ | operator. **Never 6379 / 6380 / 6381.** |
| Staged GGUFs (`granite-embedding-311m-multilingual-r2` Q4_K_M, `bge-reranker-v2-m3` Q5_K_M) | operator |
| A box with no other llama.cpp process for the duration | operator |

The two engineering gaps in that table are the real critical path. The
API key blocks the run; the missing A1 arm and the missing token capture
block the *design* from being executable at all, and neither is a
five-minute change. Sequence them first.

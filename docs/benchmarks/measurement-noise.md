# Measurement noise — what is evidenced, and what is not

Two figures have been used to decide whether a benchmark delta is real.
One is well evidenced and stays. The other has no source and is retired
here, on 2026-08-21 (W3.10).

Keeping them on the same page is deliberate. The failure mode this
document guards against is not "we had no noise estimate" — it is "we had
a number that *sounded* like a noise estimate, cited it in decisions, and
nobody could say where it came from".

---

## 1. The judge/generation noise floor: **± ~5 points**. Evidenced — keep it.

**Claim.** On the LLM-judged LongMemEval J-score, a re-run of an
*identical* configuration can move the score by roughly 5 points in
either direction, for reasons that have nothing to do with retrieval.

**Evidence.** Recorded 2026-07-30 and written down in
[`scripts/bench/lme/README.md`](../../scripts/bench/lme/README.md) §3
("The judge/generation noise floor is about ±5 points"):

> the same config re-run produced **byte-identical retrieval on 108/108
> questions and still flipped 10 verdicts**. Temperature is already 0 on
> both the generator and the judge; the remaining variance is the
> provider's.

That is the right shape of evidence for a noise claim: the variable under
test was pinned (retrieval was proven identical question-by-question,
not merely assumed), the obvious confound was eliminated (temperature 0
on both legs), and the residual was counted rather than estimated. 10
flips over 108 questions is 9.3 points of churn; ±5 is a conservative
half-width, not a rounding of it.

**Consequences — these are load-bearing rules, not advice.**

- A sub-5-point J delta **is not signal**. Do not chase it, ship on it,
  or write it in a changelog.
- The control for an arm is **the other arm of the same run**. Never a
  number from a previous day. This is why `ab_run.sh` runs graph-off and
  graph-on back to back over the same offsets.
- A partial arm is not comparable to a complete one. `tally.py` prints
  `RUN NOT FINAL` until coverage is complete and `ERR == 0`.
- ERR is not a wrong answer. A judge outage counted as misses once made a
  20-question provider failure look like a 5-point regression.

**Where the floor does *not* apply:**

- **The CI recall ratchet (any-gold).** No LLM judge is involved — the
  metric is "was a gold-evidence session present in the capped reader
  context", computed from the retrieval trace. It is deterministic, so a
  two-question drop there is signal at a scale where a two-question J
  drop would not be. That is the entire reason the ratchet uses any-gold.
- **PersonaMem.** Scoring is exact letter match; there is no judge. (The
  *reader* is still a hosted model — see §2.)

---

## 2. "~3.2 points of reader drift across days": **RETIRED — unsubstantiated.**

**The claim, as it was being used.** That the same PersonaMem reader
model, given the same prompt on different days, moves by about 3.2
points — and that this invalidates any comparison between arms run on
different days.

**Verdict: retired. It must stop being cited, in any document, issue,
commit message or agent-facing note.** Not because it is implausible —
it is entirely plausible — but because we searched for its source and
there isn't one:

| Searched | Result |
|---|---|
| `docs/**` | no hit |
| `scripts/**` | no hit |
| `README.md`, `CLAUDE.md` | no hit |
| git log message bodies, all branches | no hit |
| GitHub issues (all states) | no hit |
| GitHub pull requests (all states) | no hit |

No run produced it that we can find, no artifact set supports it, no
arms are named, no N is stated, and no date is attached. A figure with
that provenance is indistinguishable from one that was invented, and it
was being used to *discard* real measurements — which is the most
expensive possible use for an unverifiable number.

**What survives, and why the practice does not change.** The operational
rule the drift figure was invoked to justify — *arms must be compared
within a run, never across days* — is already fully supported by §1,
which has evidence. Nothing about how we run A/Bs needs to change; only
the citation does. Interleave arms per context, use the other arm of the
same run as the control, and the question of how much a reader moves
overnight never has to be answered.

**If someone wants the figure back**, the bar is the §1 bar: pin the
variable under test (same questions, same retrieved context — ideally
replayed from committed artifacts so retrieval is provably identical),
re-run the reader on two separate days, count the flips, state N and the
dates, and commit the raw artifacts under
[`pm-raw/`](pm-raw/README.md). Until that exists, there is no
reader-drift figure.

**The real, unmeasured risk it was gesturing at is worth naming**: our
PersonaMem readers (`claude-sonnet-5`, `claude-opus-5`) and our
LongMemEval generator/judge (`minimax-m3:cloud`) are *hosted* models that
can change underneath a benchmark without any change on our side. That is
a genuine hazard for any absolute score published over time. The honest
current statement is: **we have not measured it, and we do not have a
number for it.**

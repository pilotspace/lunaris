# 08 · Step 6 — Verify

[← 07 Step 5 Build](./07-step-5-build.md) · [Contents](./README.md) · Next: [09 The loop →](./09-the-loop.md)

> **Purpose:** confirm the result is correct and safe to release.
> **Produces:** a reviewed change with a recorded outcome, ready to release.
> **Who resolves it:** set per task by the `autonomy:` header. Under `autonomy: auto` (the default) the run resolves the gate on evidence; under `conservative`, or for any residue, it is the human's check. **Security always escalates to a human.**

---

## Where trust is actually established

The build produced passing tests. That is necessary but not sufficient. Verification is where a person establishes trust — and the principle governing it is *trust through evidence, not inspection.*

This needs care, because it is easy to misread. "Not by inspection" does not mean "do not look at the code." It means the *basis* of trust is the passing evidence plus a deliberate check of the specific things tests cannot easily catch — not a general impression that the code reads plausibly. Plausibility is exactly the trap: AI code is frequently plausible and wrong. So verification has two parts: confirm the evidence, then check the known non-functional risks.

## Who resolves Verify — the automated quality gate

Verify can be resolved two ways, set per task by the `autonomy:` header (see [governance](./11-governance.md) and the autonomy level):

- **Auto (the default).** When `autonomy: auto`, the run resolves the gate on **evidence** rather than waiting for a person — but only when *all* of these hold: every test green, coverage not decreased, no test weakened and no contract edited, the convergence loops dry, and **no residue** (security, concurrency, or architecture). It records `PASS` as *auto-resolved*, naming the run as the accountable owner — an explicit pass, not a skip. This is principle 7: a gate may be resolved by evidence when that evidence is sufficient and the result is logged.
- **Human.** When `autonomy: conservative`, or whenever the run finds residue it cannot judge, the gate stops for a person; the two parts below are theirs.

**Security is always a `HARD-STOP` and is never auto-passed, at any autonomy level.** The two parts that follow — confirm the evidence, then check the non-functional risks — are what *either* resolver works through; the only question is whether a person or the recorded run signs the outcome.

## Part one — confirm the evidence

- [ ] All tests pass.
- [ ] Coverage did not decrease.
- [ ] No test or contract was altered during the build.

If any of these is false, stop here and return to the build; there is nothing to verify yet.

## Part two — check what tests miss

Automated tests are excellent at behavior on defined inputs and poor at a few specific things. Check those by hand, every time:

- **Concurrency and timing.** Is the operation correct when two of them happen at once? Tests usually run serially and miss races.
  - ▶ *Example: the balance update must be one atomic transaction. Confirm that two simultaneous transfers from the same account cannot both pass the balance check and overdraw it.* This is the single most important check for this feature, and it is the reason the build prompt named atomicity explicitly.
- **Security.** Are there exposed secrets, injection openings, or unexpected dependencies? AI-generated code is known to hardcode secrets and to pull in packages by plausible-but-wrong names.
- **Architecture conformance.** Does the change respect the layering and dependency rules in `CONVENTIONS.md`? Speed with no architectural check produces a fast-growing tangle that becomes unmaintainable within months.

## Part three — the deep check (do not skim)

Two failures slip straight past green tests. The first is code that is never *wired in* — a new function that nothing calls, an endpoint no route reaches: the tests for it pass in isolation while the feature is, in practice, absent. The second is the opposite — code left *dead* behind a path nothing exercises, quietly rotting. And for a change that produced prose rather than code, the equivalent failure is signing off on a claim you never actually read in full. Plausibility hides all three. So verification carries one explicit requirement beyond the non-functional review:

> Deep check — do not skim. If the task produced code, record that every new symbol is referenced (wiring) and that no new dead/unused code was introduced. If it produced prose or non-code, record a semantic read — what you read in full and what it confirmed. Which path applies is the resolver's judgement; the engine never classifies.

This is *evidence*, not impression: a reference search showing where each new symbol is called, a scan confirming nothing new is orphaned, or — for prose — a note of exactly what was read and what it confirmed. An unfilled deep check is a **shallow verify**, not a pass. The engine cannot judge wiring, dead code, or whether prose was truly read; the resolver records the evidence, and a person (under `conservative`) or the recorded run (under `auto`) signs it.

## Recording the outcome

Every verification ends with exactly one recorded outcome, with an accountable owner — never a silent pass:

| Outcome | Meaning | Allowed when |
|---------|---------|--------------|
| `PASS` | all checks met | the normal path |
| `RISK-ACCEPTED` | proceed with a signed waiver: named owner, linked ticket, expiry date | a non-security gap only |
| `HARD-STOP` | cannot proceed | any failing test or any security finding |

A security finding is always a `HARD-STOP`; it is never waved through with a waiver. A `RISK-ACCEPTED` outcome is a deliberate, documented decision to ship a known, non-security limitation — not a way to skip the check.

## The verification checklist

- [ ] All tests pass (the evidence).
- [ ] Concurrency/timing of the risky operation is safe.
- [ ] No exposed secrets, injection openings, or unexpected dependencies.
- [ ] Layering and dependencies follow `CONVENTIONS.md`.
- [ ] Deep check (do not skim): for code, every new symbol is referenced (wiring) and no new dead/unused code was introduced; for prose/non-code, a semantic read is recorded.
- [ ] The change is approved — by a person, **or** (under `autonomy: auto`, no residue) auto-resolved by the run as the recorded accountable owner.
- [ ] An outcome is recorded (`PASS` / `RISK-ACCEPTED` / `HARD-STOP`).

## Common mistakes

- **Shipping on plausibility.** Reading the diff, finding it reasonable, and approving — without the evidence and the non-functional review — is the precise failure the method exists to prevent.
- **Treating a security gap as acceptable risk.** It is a `HARD-STOP`, not a waiver.
- **Skipping the concurrency check** because the tests are green. Tests rarely exercise simultaneity; this is a manual check by design.

## If the check fails

A failing test or a security finding returns the change to the build step ([Step 5](./07-step-5-build.md)). A non-security limitation may proceed only with a signed `RISK-ACCEPTED` record carrying an owner and an expiry — so the team can find and close it later. Nothing proceeds on an unrecorded decision.

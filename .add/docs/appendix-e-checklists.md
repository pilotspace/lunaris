# Appendix E · Checklists

[← Appendix D Worked example](./appendix-d-worked-example.md) · [Contents](./README.md) · Next: [Appendix F Requirements matrix →](./appendix-f-requirements-matrix.md)

Every exit check in the book, collected for quick use. Print this page.

---

## Setup (once per project)

- [ ] Pipeline runs and is green on the empty skeleton.
- [ ] AI model pinned in `MODEL_REGISTRY.md`.
- [ ] Dependency allow-list exists; pipeline fails on anything outside it.
- [ ] `playbook/` contains the six prompts.

## Step 1 — Specify

- [ ] Every required behavior stated explicitly.
- [ ] Every rejection has a named error code.
- [ ] Success state-change described.
- [ ] Assumptions ranked lowest-confidence first; the 1–2 most-likely-wrong ⚠-flagged with why + cost (or an honest "none material" that still names the single biggest risk).

## Step 2 — Scenarios

- [ ] Every "Must" rule has a scenario.
- [ ] Every "Reject" rule has a scenario.
- [ ] Each result is a specific, observable fact.
- [ ] Rejections assert what must stay unchanged.

## Step 3 — Contract

- [ ] Contract versioned and `FROZEN`.
- [ ] Contract tests pass against the mock.
- [ ] Names match the glossary.
- [ ] Every spec rejection has a contracted response.

## Step 4 — Tests

- [ ] One test per scenario.
- [ ] Suite runs in the pipeline and is red for the right reason.
- [ ] Tests assert behavior, not internals.
- [ ] Coverage target recorded.

## Step 5 — Build

- [ ] All tests pass.
- [ ] Coverage did not decrease.
- [ ] No test or contract modified by the AI.
- [ ] No package outside the allow-list added.
- [ ] Change is small enough to review in full.

## Step 6 — Verify

- [ ] All tests pass (the evidence).
- [ ] Concurrency/timing of the risky operation is safe.
- [ ] No exposed secrets, injection, or unexpected dependencies.
- [ ] Layering and dependencies follow `CONVENTIONS.md`.
- [ ] A person reviewed and approved.
- [ ] Outcome recorded (`PASS` / `RISK-ACCEPTED` / `HARD-STOP`).

## The loop

- [ ] Released behind a flag or gradual rollout.
- [ ] Scenarios reused as production monitors.
- [ ] Learnings written back as a `SPEC.md` delta.

---

## Master shippable checklist

A feature is shippable only when all are true:

- [ ] Spec complete: behavior stated, rejections named, assumptions ranked lowest-confidence first with the biggest risk flagged.
- [ ] Every rule has a scenario.
- [ ] Contract frozen; contract tests green.
- [ ] A test per scenario; suite was red before the build.
- [ ] All tests green; coverage held; tests and contract untouched by the AI.
- [ ] Concurrency, security, and architecture checked by a person.
- [ ] Gate outcome recorded with an accountable owner.
- [ ] Released behind a flag, with monitors in place.

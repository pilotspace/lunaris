# Appendix D · The worked example, end to end

[← Appendix C Glossary](./appendix-c-glossary.md) · [Contents](./README.md) · Next: [Appendix E Checklists →](./appendix-e-checklists.md)

The running example, assembled in one place so you can see a complete pass through the flow without flipping between chapters. The feature: **transfer money between a user's own accounts.**

---

## Step 1 — Specify → `SPEC.md`

```
Feature: Transfer money between my own accounts
Framings weighed: synchronous single-currency transfer (chosen) · queued transfer · multi-currency with FX
Must:
  - move an amount from one of my accounts to another of mine
  - amount > 0
  - source and destination are different accounts
  - source has enough balance
After:
  - source balance -= amount, destination balance += amount
Reject:
  - amount <= 0           -> "amount_invalid"
  - source == destination -> "same_account"
  - balance < amount      -> "insufficient_funds"
  - account not mine      -> "forbidden"
Assumptions — lowest-confidence first:
  ⚠ same currency only (no FX) in v1 — lowest confidence because the ticket never said; if wrong: the amount/rounding model changes and this contract is wrong
  - [x] no daily limit in v1 — confirmed: out of scope for v1
```

The product owner read the flagged assumption first — the single-currency choice, the one most likely to be wrong and most expensive if it were — and confirmed it: v1 is single-currency with no daily limit.

## Step 2 — Scenarios → `features/transfer.feature`

```
Scenario: successful transfer
  Given A has 100 and B has 0, both mine
  When I transfer 30 from A to B
  Then A has 70 and B has 30

Scenario: amount must be positive
  Given A has 100, mine
  When I transfer 0 from A to B
  Then it is rejected "amount_invalid"
  And no balance changes

Scenario: same account
  Given A has 100, mine
  When I transfer 10 from A to A
  Then it is rejected "same_account"
  And no balance changes

Scenario: insufficient funds
  Given A has 20, mine
  When I transfer 50 from A to B
  Then it is rejected "insufficient_funds"
  And no balance changes

Scenario: not my account
  Given account C is not mine
  When I transfer 10 from C to B
  Then it is rejected "forbidden"
```

Five scenarios for four rejections plus the happy path — every rule from the spec is covered.

## Step 3 — Contract → `contracts/transfer.md`

```
POST /transfers   body: { fromAccountId, toAccountId, amount }
  200 -> { transferId, fromBalance, toBalance }
  400 -> { error: "amount_invalid" | "same_account" | "insufficient_funds" }
  403 -> { error: "forbidden" }
Schema: accounts.balance (read + write, must be transactional)
Status: FROZEN @ v1
```

Frozen at v1. The schema note flags the atomicity requirement the verification step will check.

## Step 4 — Tests → `tests/transfer_test.py` (run first; all fail)

```python
def test_successful_transfer():
    a = account(balance=100, owner=me); b = account(balance=0, owner=me)
    r = transfer(a.id, b.id, 30)
    assert r.status == 200
    assert a.balance == 70 and b.balance == 30

def test_amount_must_be_positive():
    a = account(balance=100, owner=me); b = account(balance=0, owner=me)
    r = transfer(a.id, b.id, 0)
    assert r.status == 400 and r.error == "amount_invalid"
    assert a.balance == 100 and b.balance == 0

def test_same_account():
    a = account(balance=100, owner=me)
    r = transfer(a.id, a.id, 10)
    assert r.status == 400 and r.error == "same_account"
    assert a.balance == 100

def test_insufficient_funds():
    a = account(balance=20, owner=me); b = account(balance=0, owner=me)
    r = transfer(a.id, b.id, 50)
    assert r.status == 400 and r.error == "insufficient_funds"
    assert a.balance == 20

def test_not_my_account():
    c = account(balance=100, owner=someone_else); b = account(balance=0, owner=me)
    r = transfer(c.id, b.id, 10)
    assert r.status == 403 and r.error == "forbidden"
```

Run now, with no implementation: all five fail. That is the honest baseline.

## Step 5 — Build → the prompt given to the AI

```
Read SPEC.md, contracts/transfer.md, and tests/transfer_test.py.
Implement POST /transfers so that EVERY test passes.
Constraints:
  - Do NOT change any test.
  - Do NOT change the contract.
  - Make the balance update atomic: debit and credit in a single transaction,
    and re-check the balance inside the transaction.
  - Stop and ask if any requirement is unclear — do not guess.
  - Use only packages in dependencies.allowlist.
Report which tests pass and exactly what you changed.
```

The AI implements, runs the suite, iterates, and reports all five green, listing the files it changed.

## Step 6 — Verify → the human checks

- **Evidence:** all five tests pass; coverage held; no test or contract was altered. ✓
- **Concurrency (the key check):** two simultaneous transfers from account A must not both pass the balance check and overdraw it. The reviewer confirms the balance re-check happens *inside* the transaction and that the row is locked for the update — so a race cannot double-spend. ✓
- **Security:** no hardcoded secrets; inputs validated; no new dependency added. ✓
- **Architecture:** the change respects the layering in `CONVENTIONS.md`. ✓
- **Outcome recorded:** `PASS`, reviewed by the senior engineer.

## The loop — observe

Released behind a feature flag to 5% of users. Monitored:

- transfer error rate (target: well under 0.1% of attempts);
- the rate of each rejection — a spike in `insufficient_funds` would suggest a UX problem (users not seeing their balance) rather than a code defect;
- latency of the atomic update under load.

A week later, telemetry shows an unexpectedly high `forbidden` rate. The `6_observe` prompt clusters it: users are trying to transfer *into* a shared account they can see but do not own. That observation becomes a `SPEC.md` delta — "support transfers into accounts I am authorized on, not only accounts I own" — and the flow returns to Step 1 for the next cycle.

---

This is the whole method in one feature: four artifacts written in order, an AI build bounded by them, a verification grounded in evidence plus the one check tests miss, and a loop that turns production reality into the next specification.

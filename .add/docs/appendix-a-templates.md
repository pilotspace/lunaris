# Appendix A · Templates

[← 15 Foundations & Lineage](./15-foundations-and-lineage.md) · [Contents](./README.md) · Next: [Appendix B Prompts →](./appendix-b-prompts.md)

Copy-paste blanks. Project-level templates are filled once at setup; feature-level templates are filled once per feature.

---

## Project-level (set up once)

### `CONVENTIONS.md`
```
Language/framework: <e.g. Python 3.12 / FastAPI>
Folders: src/  tests/  contracts/  features/  playbook/
Naming: <file case>, <type case>, verbs for functions
Lint/format: <tools>, enforced in the pipeline
Errors: machine-readable error codes (string enums), never free text
Architecture: <layering and dependency rules>
```

### `MODEL_REGISTRY.md`
```
Model: <name>
Version: <version/date>
Adopted: <date>
Notes: re-run the prompt golden-cases before changing this.
```

### `dependencies.allowlist`
```
# one package per line; the pipeline rejects anything not listed
<package>==<version-or-range>
```

---

## Feature-level (once per feature)

### `SPEC.md`
```
Feature: <name>
Framings weighed: <chosen> (chosen) · <alternative> · <alternative>
Must:
  - <required behavior>
Reject:
  - <bad input / situation> -> "<error_code>"
After:
  - <state true once it succeeds>
Assumptions — lowest-confidence first:
  ⚠ <most-likely-wrong assumption> — lowest confidence because <why>; if wrong: <cost>
  - [x] <confirmed / low-stakes assumption> — <one line>
```

### `features/<name>.feature`
```
Scenario: <short name>
  Given <starting situation>
  When <action>
  Then <expected result>
  And <what must remain unchanged>   # when relevant
```

### `contracts/<name>.md`
```
<METHOD> <path>   body: { <fields> }
  200 -> { <success fields> }
  4xx -> { error: "<code>" | "<code>" }
Schema: <tables/fields touched, and access pattern>
Status: FROZEN @ v<n>
```

### `tests/<name>_test.<ext>` (stub)
```
test_<scenario_name>:
  arrange: <set up the Given>
  act:     <do the When>
  assert:  <check the Then>
  assert:  <check what must stay unchanged>   # when relevant
```

### Gate outcome record
```
Feature: <name>   Step: <Specify|...|Verify>   Date: <date>
Reports: Test=<pass/fail>  Quality=<pass/fail>  Risk=<summary>
Outcome: <PASS | RISK-ACCEPTED | HARD-STOP>
If RISK-ACCEPTED -> owner: <name>  ticket: <link>  expires: <date>
Reviewed by: <name>
```

# UDD tokens — the compact-DTCG dialect

The token foundation a UI project drafts from. Three layers, a fail-closed
citation rule, and value forms compacted for the AI economy. Aligned with the
**Design Tokens Format Module 2025.10** (Final Community Group Report, 28 Oct 2025
— <https://www.designtokens.org/tr/2025.10/format/>); every divergence is NAMED
below. `tokens.sample.json` is a worked example that validates clean.

## Identity values are human-owned — discuss at specify

The dialect checks a token's **shape**; its identity **value** — the brand
color, the palette seed, the typeface — is design *direction*, not an AI
default. During **specify**, surface identity tokens for discussion with the
user; never invent a brand value. Shape is verifiable; identity is a human
decision.

## The three layers (a convention, not a DTCG feature)

A token's **layer is its top-level group name** — `primitive`, `semantic`, or
`component`. DTCG itself defines groups, `$type`, and aliases but no tier model;
the layering and the citation rule below are ours.

```
primitive   raw literal values        (no alias — the source of truth)
semantic    intent, cites primitives  (e.g. color.accent → primitive.color.blue-500)
component   parts, cite semantics      (e.g. button.bg → semantic.color.accent)
```

## The citation rule (fail-closed)

- a `primitive` token's `$value` MUST be a literal — never an alias.
- a `semantic` token's `$value` MUST be a literal OR an alias to a `primitive` only.
- a `component` token's `$value` MUST be a literal OR an alias to a `semantic` only.

No layer-skipping, no sideways or upward citation. An **alias** is the DTCG curly
form `"{layer.dotted.path}"` used as a `$value`; chains are allowed (each hop is
checked); the chain must terminate in a literal.

## Token shape (kept from DTCG)

- a **token** is an object with `$value` (required); optional `$type`,
  `$description`.
- `$type` is optional on a token and **inherited** from the nearest parent group
  that sets it.
- a **group** is any object without `$value`.

## Value forms — supported `$type`s (NAMED divergences from DTCG 2025.10)

| `$type`      | compact form (ours)                    | DTCG 2025.10 form        |
|--------------|----------------------------------------|--------------------------|
| `color`      | `"#RRGGBB"` / `"#RRGGBBAA"`            | object `{colorSpace,…}` ⟵ DIVERGES |
| `dimension`  | `"<n><unit>"`, unit ∈ px·rem·em·%·vh·vw | object `{value,unit}` ⟵ DIVERGES   |
| `number`     | JSON number                            | JSON number              |
| `fontWeight` | `100`–`900` or a keyword string        | number / keyword         |
| `duration`   | `"<n>ms"` / `"<n>s"`                    | object `{value,unit}` ⟵ DIVERGES   |
| `fontFamily` | string or array of strings             | string / array           |

Composites (`shadow`, `typography`, `border`) and the rarer DTCG types
(`cubicBezier`, `strokeStyle`, `gradient`, `transition`) are **deferred** —
additive later without breaking this dialect.

## Validation — the named reds

`_token_layer_violations(tokens)` returns `[]` for a valid file, else one
`(code, path, detail)` per violation. The six codes:

| code | when |
|------|------|
| `unknown_layer` | a top-level group is not primitive/semantic/component |
| `unknown_type` | a token's resolved `$type` is outside the supported set |
| `unresolved_alias` | `{a.b.c}` points at no token bearing a `$value` |
| `cross_layer_citation` | an alias skips or inverts a layer |
| `primitive_has_alias` | a primitive token's `$value` is an alias, not a literal |
| `malformed_value` | a literal `$value` does not match the form for its `$type` |

The validator lints **shape only** — stdlib, tool-agnostic, no rendering. The
`add.py check` wiring (named reds) and the catalog/prototype-tree rules arrive in
later udd-design-foundation tasks. To render: point a json-render-style renderer
at the component layer (see the milestone's render recipe).

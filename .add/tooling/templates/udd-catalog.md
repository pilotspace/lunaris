# UDD catalog + content trees — the render-ready half

The token foundation (`udd-tokens.md`) says what a UI is *made of*; this layer says
what it is *built from* and *laid out as*. A **catalog** declares the components and
their typed props; a **content tree** is a flat, json-render-shaped prototype that
binds those props to semantic tokens. `catalog.sample.json` + `prototype.sample.json`
are a worked pair that validates clean against each other.

## The foundation is a NAMED SET (Fork A)

```
.add/design/tokens.json            the task-1 dialect (primitive · semantic · component)
.add/design/catalog.json           the component catalog (this doc)
.add/design/prototypes/<name>.json one flat content tree per prototype screen/flow
```

Each file lints independently — the token validator never sees the catalog, the
catalog/tree validator never sees the tokens. The cross-file check (a tree's
token-prop actually resolving to a semantic token of the right `$type`) is the
**composer's** job — `add.py check` wires it in the udd-check-lint task.

## CATALOG — components and typed props

```
{ "components": { "<Name>": {
    "description"?: str,
    "hasChildren"?: bool        (default false — only containers may hold children),
    "props": { "<prop>": PropSpec } } } }
```

A **PropSpec** is exactly one of:

| PropSpec | the tree value it accepts |
|----------|---------------------------|
| `{ "type": "string" }`  | a JSON string |
| `{ "type": "number" }`  | a JSON number (not a bool) |
| `{ "type": "boolean" }` | a JSON bool |
| `{ "type": "enum", "values": ["a","b"] }` | a string in `values` |
| `{ "type": "token", "token": "<$type>" }` | a `{semantic.…}` alias (see below) |

`<$type>` is a task-1 token type — `color · dimension · number · fontFamily ·
fontWeight · duration`.

## CONTENT TREE — a flat json-render `Spec`

Pinned to **vercel-labs/json-render v0.19.0 / commit `4e4dc46`** (the milestone's
named top risk: young-project schema drift). The tree mirrors json-render's `Spec`
exactly, so it is render-ready as-is:

```
{ "root": "<id>",
  "elements": { "<id>": {
      "type": "<Name>",          a catalog component name
      "props": { … },            keys ⊆ the component's declared props
      "children"?: ["<id>", …]   only if the component hasChildren; ids ∈ elements
  } } }
```

- A `token` prop's value is the **alias form** `"{semantic.dotted.path}"` from the
  token dialect — it MUST target the `semantic` layer. (Whether that semantic token
  exists and matches the prop's `$type` is resolved by the composer, which has
  `tokens.json`.)
- json-render's optional `state` / `on` / `visible` / `repeat` are **passed through**
  — render-compatible (a clickable prototype) but NOT linted, keeping the validator
  lean. Interactivity rules stay additive for a later task.

## Validation — the named reds

`_catalog_tree_violations(catalog, tree)` returns `[]` for a valid pair, else one
`(code, path, detail)` per violation, in deterministic order. The eight codes:

| code | when |
|------|------|
| `tree_cites_uncataloged_component` | an element `type` is not in the catalog |
| `unknown_prop` | a props key not declared on the element's component |
| `prop_type_mismatch` | a value's form ≠ its PropSpec (incl. a token prop given a non-alias literal) |
| `non_semantic_prop_token` | a token-prop alias does not target the `semantic` layer |
| `dangling_child` | a child id absent from `elements` |
| `children_not_allowed` | children on a component whose `hasChildren` is false |
| `missing_root` | `root` absent, or names an id not in `elements` |
| `malformed_catalog` | a PropSpec with unknown `type`, or a token prop naming an unknown `$type` |

The validator lints **shape only** — pure, stdlib, tool-agnostic, no rendering. It is
SEPARATE from `_token_layer_violations`; udd-check-lint composes both inside
`add.py check`.

## Render recipe — catalog.json → json-render

json-render authors a catalog in TypeScript (`defineCatalog`), not JSON, so our
`catalog.json` is adapted by a thin (~20-line) loader. The content tree needs no
adapter — it is a json-render `Spec` already:

```ts
import { defineCatalog } from "json-render";
import { z } from "zod";
import catalogJson from "./catalog.json";
import spec from "./prototypes/welcome.json";

const PROP = {                       // PropSpec → a Zod field
  string:  () => z.string(),
  number:  () => z.number(),
  boolean: () => z.boolean(),
  enum:    (p) => z.enum(p.values),
  token:   () => z.string(),         // "{semantic.…}" — resolved to a CSS value at render
};

const components = Object.fromEntries(
  Object.entries(catalogJson.components).map(([name, c]) => [name, {
    hasChildren: c.hasChildren ?? false,
    props: z.object(
      Object.fromEntries(Object.entries(c.props).map(([k, p]) => [k, PROP[p.type](p)])),
    ),
  }]),
);

const catalog = defineCatalog(z, { components });
catalog.validate(spec);              // the flat tree feeds json-render AS-IS
```

Resolving a `{semantic.…}` alias to a concrete CSS value (via `tokens.json`) is the
renderer's binding step — out of scope for the foundation, which only guarantees the
SHAPE is render-ready. The renderer itself is never bundled with the method.

// Lunaris — public TypeScript declarations for `@pilotspace/lunaris`
// (package `types`).
//
// HAND-WRITTEN. `napi build` generates `index.d.ts`; this file re-exports
// that surface and adds the ergonomic layer implemented in `lunaris.cjs`.
//
// `index.d.ts` used to be the package's `types`, which is why the README
// quickstart did not type-check for four minor versions: `open` and
// `RetrievalBuilder.bind` / `.query` / `.execute` exist only in the
// hand-written layer, and every `napi build` overwrote any attempt to
// declare them there. See lunaris.cjs's header for why the PUBLIC entry
// moved instead of the generated one.
//
// Rules for editing:
//   * anything emitted from a `#[napi]` item belongs in `index.d.ts` and is
//     regenerated — never hand-add it here;
//   * anything implemented in `lunaris.cjs` belongs here, and MUST be kept
//     in step with it. `npm run typecheck` compiles the README quickstart
//     against this file; `npm test` exercises the runtime.

// Everything the binding generates. Names re-declared below shadow their
// generated counterparts (`Vector`, `Keyword`, `Graph`, `RetrievalBuilder`).
export * from "./index.js";

import type {
  Lunaris as NativeLunaris,
  ScopedLunaris as NativeScopedLunaris,
} from "./index.js";

/**
 * A handle returned by {@link open}.
 *
 * Structurally the generated `Lunaris` class, minus `recall()`, which
 * `index.js` rebinds to the working {@link RetrievalBuilder} (the generated
 * one is a codegen stub whose every method throws), plus the accessors
 * {@link open} installs.
 */
export interface LunarisHandle extends Omit<NativeLunaris, "recall"> {
  /** A {@link RetrievalBuilder} pre-bound to this handle. */
  recall(): RetrievalBuilder;
  /** Multi-agent partition view over this handle (RFC 0001). */
  scoped(scope: unknown): ScopedHandle;
  /** Graph-extraction pipeline toggle. */
  graphPipeline: GraphPipelineHandleExt;
  /** ACT-R consolidator toggle. */
  consolidatorPipeline: ConsolidatorPipelineHandleExt;
}

/** Package version, from package.json. */
export declare const __version__: string;

/** napi-rs maps a Rust error to a plain JS `Error` whose message is
 * `"CODE: message"` (e.g. `"STORAGE: …"`). This alias exists so
 * `err instanceof LunarisError` reads naturally; it IS `Error`. */
export declare const LunarisError: ErrorConstructor;

/** Config-surface toggles accepted by {@link open}. */
export interface OpenOptions {
  graphPipeline?: { enabled?: boolean };
  consolidatorPipeline?: { enabled?: boolean };
}

/** `.enable()` / `.disable()` / `.isEnabled()` / `.toggle(on)` layered over
 * the codegen-single toggle surface. Reachable as `handle.graphPipeline`
 * after {@link open}. */
export declare class GraphPipelineHandleExt {
  constructor(rust: unknown);
  enable(): void;
  disable(): void;
  isEnabled(): boolean;
  toggle(on: boolean): void;
}

/** Consolidator twin of {@link GraphPipelineHandleExt}. Reachable as
 * `handle.consolidatorPipeline` after {@link open}. */
export declare class ConsolidatorPipelineHandleExt {
  constructor(rust: unknown);
  enable(): void;
  disable(): void;
  isEnabled(): boolean;
  toggle(on: boolean): void;
}

/** A hydrated retrieval hit (`lunaris_retrieve::Hit`). */
export interface Hit {
  id: number[];
  score: number;
  /** Chunk text body. */
  text: string;
  /** Episode source, e.g. `helios:fs/notes.md`. */
  source: string;
  heading_path: string[];
  valid_from: unknown;
  valid_to: unknown | null;
  degraded: boolean;
  rerank_applied: boolean;
  source_op: string;
  [key: string]: unknown;
}

/** Equality predicate object accepted by `.filter()`, ANDed together. */
export type FilterSpec = string | Record<string, string | number | boolean>;

/** Combinators shared by every DSL entry point. Each returns a
 * {@link RetrievalBuilder}, carrying the bound handle (if any) down the
 * chain. */
export declare class Composable {
  /** Compose `other` in parallel with the current plan; unions the results. */
  and(other: Composable): RetrievalBuilder;
  /** Fuse parallel branches via reciprocal-rank fusion. */
  fuseRrf(k: number): RetrievalBuilder;
  /** Keep only the top `n` hits. Wins over the leaf operator's `k`. */
  top(n: number): RetrievalBuilder;
  /**
   * Set the query text the plan searches for — the TS spelling of the Rust
   * terminal `builder.execute(Query::text(t))`.
   *
   * Without it the plan searches for the empty string.
   */
  query(text: string): RetrievalBuilder;
  /** Pin the query to an as-of wall-clock view (SQL:2011 bi-temporal read). */
  asOf(wallMs: number | bigint): RetrievalBuilder;
  /** Attach a filter predicate: a string, or an object of ANDed equalities. */
  filter(pred: FilterSpec): RetrievalBuilder;
  /** Attach a raw filter-predicate string. */
  filterStr(s: string): RetrievalBuilder;
}

/** Dense-vector leg over `index`, taking `k` candidates. */
export declare class Vector extends Composable {
  constructor(index: string, k: number);
  static new(index: string, k: number): Vector;
}

/** BM25 keyword leg over `index`, taking `k` candidates. */
export declare class Keyword extends Composable {
  constructor(index: string, k: number);
  static bm25(index: string, k: number): Keyword;
}

/** Graph leg anchored on `entityIds`, traversing up to `hops` edges. */
export declare class Graph extends Composable {
  constructor(entityIds: readonly string[], hops?: number);
  static anchored(entityIds: readonly string[], hops?: number): Graph;
}

/**
 * Chainable retrieval plan builder. Bind it to a handle — with
 * `.bind(handle)` or by starting from `handle.recall()` — then terminate
 * with `.execute()`.
 *
 * ```ts
 * const hits = await new RetrievalBuilder()
 *   .bind(handle)
 *   .query("what does Alice like?")
 *   .top(5)
 *   .execute();
 * ```
 */
export declare class RetrievalBuilder extends Composable {
  constructor(handle?: NativeLunaris, scope?: unknown);
  /** Attach a handle so `.execute()` has storage access. Returns `this`. */
  bind(handle: NativeLunaris): RetrievalBuilder;
  /** Bind the plan to a partition. Returns `this`. */
  withScope(scope: unknown): RetrievalBuilder;
  /** Run the plan. Throws if no handle was bound. */
  execute(): Promise<Hit[]>;
}

/**
 * A partition view returned by {@link LunarisHandle.scoped}.
 *
 * Structurally the generated `ScopedLunaris`, minus `dsl()`, which
 * `lunaris.cjs` rebinds to the working {@link RetrievalBuilder} pre-bound to
 * BOTH the handle and this scope — the generated one is the same codegen stub
 * `recall()` was, with no `query` and no `execute` (W4.12).
 */
export interface ScopedHandle extends Omit<NativeScopedLunaris, "dsl"> {
  /** A {@link RetrievalBuilder} bound to this handle AND this partition. */
  dsl(): RetrievalBuilder;
}

/**
 * Open a Lunaris handle against `url` (`moon://host:port`) and install the
 * `.graphPipeline` / `.consolidatorPipeline` accessors.
 *
 * ```ts
 * const handle = await open("moon://127.0.0.1:6380");
 * ```
 */
export declare function open(url: string, opts?: OpenOptions): Promise<LunarisHandle>;

/** Internal: collapse an operator tree to the flat FFI plan. Exported for
 * the parity tests only; not part of the supported surface. */
export declare function _collapsePlan(root: unknown): Record<string, unknown>;

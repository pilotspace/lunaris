// Plan 08-03 Task 2 — BIND-TS-03 TS DSL parity with the Rust surface.
//
// All tests are offline — the classes here are purely TypeScript-side
// plan-builders that collect the composed operator tree; no backend is
// required to exercise `.and(...)`, `.fuseRrf(k)`, `.top(n)`, `.asOf(ms)`,
// `.filter({...})`, `.filterStr(s)` shape-wise. The `.execute()` terminal
// is exercised in `open_ingest_recall.spec.mts`.

import { describe, expect, test } from "vitest";

const { Vector, Keyword, Graph, RetrievalBuilder } = await import(
  "../index.mjs"
);

describe("TS retrieve DSL shape parity with Rust", () => {
  test("composeVectorKeywordFuseTop — Vector.and(Keyword).fuseRrf(60).top(5) returns a RetrievalBuilder", () => {
    const plan = Vector.new("chunks", 30)
      .and(Keyword.bm25("chunks", 30))
      .fuseRrf(60)
      .top(5);
    expect(plan).toBeInstanceOf(RetrievalBuilder);
  });

  test("graphAnchoredAndAsOf — Graph.anchored(...).and(Vector(...)).asOf(ms).top(5) returns a RetrievalBuilder (asOf accepts number or bigint)", () => {
    const planNum = Graph.anchored(["alice", "bob"], 2)
      .and(Vector.new("chunks", 10))
      .asOf(1_000_000)
      .top(5);
    expect(planNum).toBeInstanceOf(RetrievalBuilder);

    const planBig = Graph.anchored(["alice", "bob"], 2)
      .and(Vector.new("chunks", 10))
      .asOf(BigInt(1_000_000))
      .top(5);
    expect(planBig).toBeInstanceOf(RetrievalBuilder);
  });

  test("filterKwargsAndPositional — filter(object) and filterStr(string) both return a RetrievalBuilder", () => {
    const a = Vector.new("chunks", 5)
      .filter({ source: "helios:fs/42" })
      .top(3);
    const b = Vector.new("chunks", 5)
      .filterStr("source = 'helios:fs/42'")
      .top(3);
    expect(a).toBeInstanceOf(RetrievalBuilder);
    expect(b).toBeInstanceOf(RetrievalBuilder);
  });

  test("methodNamesMatchRustSurface — every DSL class exposes and / fuseRrf / top / asOf / filter / filterStr", () => {
    // BIND-TS-03 — public method names on the TS DSL classes match the
    // Rust retrieve DSL. napi-rs convention camelCases (`fuse_rrf` →
    // `fuseRrf`, `as_of` → `asOf`); SEMANTICS + ARG ORDER are preserved
    // per the plan's <key_constraints>.
    for (const Cls of [Vector, Keyword, Graph, RetrievalBuilder]) {
      const proto = Cls.prototype;
      for (const meth of ["and", "fuseRrf", "top", "asOf", "filter", "filterStr"]) {
        expect(typeof proto[meth]).toBe("function");
      }
    }
    expect(typeof Keyword.bm25).toBe("function");
    expect(typeof Graph.anchored).toBe("function");
    // Vector.new mirrors Rust's `Vector::new(index, k)` factory.
    expect(typeof Vector.new).toBe("function");
  });
});

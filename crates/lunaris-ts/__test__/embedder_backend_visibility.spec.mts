// W0.7 — the TypeScript SDK can SEE a degraded embedder.
//
// ## Why this file exists
//
// `Lunaris::open` falls back to `NoopEmbedder` when no GGUF is reachable, and
// that fallback is silent by construction. Every vector is zeros, so hybrid
// recall collapses to BM25 plus insertion-order tie-breaks while `recall()`
// keeps returning successfully with a plausible-looking hit list.
// `NoopEmbedder::dim()` deliberately reports a non-zero dimension so existing
// index geometry stays valid — which means inspecting the *results* never
// reveals it either.
//
// Rust callers always had `lunaris::resolved_embedder_backend()`. Before this
// change `grep -rn degraded crates/lunaris-ts/src` returned nothing: an
// `npm i lunaris` user had no way to ask. `handle.embedderBackend()` is that
// way.
//
// Scenarios mirror the pytest sibling at
// `crates/lunaris-py/tests/test_embedder_backend_visibility.py` one-for-one.

import { describe, expect, test } from "vitest";
import net from "node:net";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const lunaris = await import("../index.mjs");

const DEFAULT_MOON_URL = "moon://127.0.0.1:6380";

// The tag set is API — see `EmbedderBackend::as_str` in
// crates/lunaris/src/handle.rs. A caller writing `if (backend === "noop")` is
// supported, so changing one of these is a breaking change.
const KNOWN_TAGS = ["llamacpp", "openai-remote", "ollama-remote", "noop", "unresolved"];

// The tags that mean "real vectors are NOT being produced". `unresolved` is
// deliberately absent: it means `open` never ran in this process, which is
// "unknown", not "degraded".
const DEGRADED_TAGS = ["noop"];

function resolveMoonUrl(): string {
  return process.env.LUNARIS_MOON_URL ?? DEFAULT_MOON_URL;
}

async function moonReachable(url: string): Promise<boolean> {
  const match = /^moon:\/\/([^:/]+):(\d+)/.exec(url);
  if (!match) return false;
  const host = match[1];
  const port = Number.parseInt(match[2], 10);
  return new Promise<boolean>((resolve) => {
    const sock = net.createConnection({ host, port, timeout: 400 });
    sock.once("connect", () => {
      sock.end();
      resolve(true);
    });
    sock.once("error", () => resolve(false));
    sock.once("timeout", () => {
      sock.destroy();
      resolve(false);
    });
  });
}

type SkippableCtx = { skip: (reason?: string) => void };

interface Handle {
  embedderBackend: () => string;
}

// A bare `return` from a vitest test is a PASS, so every not-run path here
// goes through an explicit, named `ctx.skip`. Mirrors `openOrSkip` in
// `open_ingest_recall.spec.mts`.
async function openOrSkip(ctx: SkippableCtx, url: string): Promise<Handle | null> {
  if (!(await moonReachable(url))) {
    ctx.skip(`Moon unreachable at ${url}`);
    return null;
  }
  try {
    return (await lunaris.open(url)) as unknown as Handle;
  } catch (err) {
    const msg = (err as Error).message ?? "";
    if (msg.startsWith("STORAGE:")) {
      ctx.skip(`the handle would not open (plain Redis without RediSearch?): ${msg}`);
      return null;
    }
    throw err;
  }
}

describe("embedder backend visibility", () => {
  test("theAccessorIsExposedOnTheHandle — offline shape check", async (ctx) => {
    // Runs offline against the declared surface, not a live handle: it fails
    // if a codegen regression stops emitting the method, which is exactly the
    // failure mode that would silently restore the W0.7 gap.
    const url = resolveMoonUrl();
    const handle = await openOrSkip(ctx, url);
    if (!handle) return;
    expect(typeof handle.embedderBackend).toBe("function");
  });

  test("openRecordsARealBackend — the discriminating test", async (ctx) => {
    // Asserting only "the value is a known tag" would pass against an accessor
    // wired to a cell nothing ever writes — `unresolved` is a known tag. So
    // the assertion that carries the weight is `!== "unresolved"`: after a
    // real `open`, the process HAS resolved a backend, and the SDK must be
    // able to say which.
    const url = resolveMoonUrl();
    const handle = await openOrSkip(ctx, url);
    if (!handle) return;

    const backend = handle.embedderBackend();
    expect(typeof backend).toBe("string");
    expect(KNOWN_TAGS).toContain(backend);
    expect(
      backend,
      "open() completed but the process reports no resolved embedder backend. " +
        "The accessor is reading a cell that resolve_embedder never writes — " +
        "the SDK is reporting 'unknown' where it should report the truth.",
    ).not.toBe("unresolved");
  });

  test("aDegradedBackendIsNameable — a caller can branch on it", async (ctx) => {
    // On a machine with a staged GGUF this asserts the healthy branch; on a
    // bare runner with no model cache it asserts the degraded branch. Either
    // way the test is never vacuous: the returned tag lands on exactly one
    // side of the degraded/healthy split, so a caller can act on it.
    const url = resolveMoonUrl();
    const handle = await openOrSkip(ctx, url);
    if (!handle) return;

    const backend = handle.embedderBackend();
    const degraded = DEGRADED_TAGS.includes(backend);
    const healthy =
      KNOWN_TAGS.includes(backend) && !degraded && backend !== "unresolved";
    expect(
      degraded !== healthy,
      `${backend} is neither clearly degraded nor clearly healthy — a caller ` +
        "cannot branch on it, which defeats the purpose of exposing it",
    ).toBe(true);
  });
  test("theTagDistinguishesDegradedFromHealthy — the anti-hardcode test", async (ctx) => {
    // The three tests above ALL pass against an accessor that returns a
    // hardcoded "llamacpp". This one cannot: it runs `open` twice in two
    // FRESH child processes — one with the staged GGUF, one with
    // LUNARIS_EMBEDDER_GGUF pointed at a path that does not exist — and
    // requires the two runs to disagree.
    //
    // Child processes are not an ergonomic choice, they are the only correct
    // one: `resolve_embedder` writes a process-global `OnceLock` on the first
    // `open`, so a second `open` in THIS process would replay the first
    // answer no matter what the environment says.
    const url = resolveMoonUrl();
    if (!(await moonReachable(url))) {
      ctx.skip(`Moon unreachable at ${url}`);
      return;
    }

    const here = path.dirname(fileURLToPath(import.meta.url));
    const script = `
      const l = await import(${JSON.stringify(path.join(here, "..", "index.mjs"))});
      const h = await l.open(${JSON.stringify(url)});
      process.stdout.write("TAG=" + h.embedderBackend());
    `;
    // Every route `resolve_default_embedder` can take. The degraded arm has to
    // close ALL of them: pointing only LUNARIS_EMBEDDER_GGUF at a missing file
    // leaves the remote routes open, and this job exports
    // LUNARIS_EMBEDDER_OPENAI_URL at a stub — so the "degraded" arm resolved
    // `openai-remote`, which is the resolver working correctly and the test
    // asserting an environment assumption.
    const EMBEDDER_ROUTES = [
      "LUNARIS_EMBEDDER_GGUF",
      "LUNARIS_EMBEDDER_DIR",
      "LUNARIS_EMBEDDER_OLLAMA_URL",
      "LUNARIS_EMBEDDER_OPENAI_URL",
      "LUNARIS_EMBEDDER_OPENAI_API_KEY",
      "LUNARIS_EMBEDDER_OPENAI_MODEL",
    ];

    const run = (env: NodeJS.ProcessEnv, closeAllRoutes = false): string => {
      const out = execFileSync(process.execPath, ["--input-type=module", "-e", script], {
        env: (() => {
          const base: NodeJS.ProcessEnv = { ...process.env };
          if (closeAllRoutes) for (const k of EMBEDDER_ROUTES) delete base[k];
          return { ...base, ...env };
        })(),
        encoding: "utf8",
        stdio: ["ignore", "pipe", "ignore"],
      });
      const m = /TAG=(\S+)/.exec(out);
      if (!m) throw new Error(`child produced no TAG= line: ${out.slice(-400)}`);
      return m[1];
    };

    const asShipped = run({});
    const withNoModel = run(
      { LUNARIS_EMBEDDER_GGUF: "/nonexistent/no-such-model.gguf" },
      true,
    );

    expect(KNOWN_TAGS).toContain(asShipped);
    expect(withNoModel).toBe("noop");

    if (asShipped === "noop") {
      // No GGUF staged on this machine, so both arms degrade and the two runs
      // legitimately agree. Say so out loud rather than reporting a green that
      // proved nothing.
      ctx.skip(
        "no embedder is reachable in the ambient environment either, so both " +
          "arms report 'noop' and this machine cannot show the two apart",
      );
      return;
    }
    expect(
      asShipped,
      "closing every embedder route changed nothing — the " +
        "accessor is not reading the resolved backend",
    ).not.toBe(withNoModel);
  });
  test("bothPublishedEntriesCarryIt — ESM and CJS, not just the binding", async (ctx) => {
    // `package.json` resolves `import` to ./index.mjs and `require` to
    // ./lunaris.cjs — two entry points, and every other test in this file
    // exercises only the first. A wrapper change that dropped the method from
    // the CJS surface would leave all of them green while `require("lunaris")`
    // users lost it, which is the 4-of-5-render-sites shape.
    //
    // `lunaris.cjs` re-exports the native class and installs accessors on the
    // handle `open` returns, so the method reaches CJS by inheritance rather
    // than by an explicit re-declaration — precisely the kind of coverage that
    // holds by accident until it doesn't.
    const url = resolveMoonUrl();
    if (!(await moonReachable(url))) {
      ctx.skip(`Moon unreachable at ${url}`);
      return;
    }

    const here = path.dirname(fileURLToPath(import.meta.url));
    const cjsEntry = path.join(here, "..", "lunaris.cjs");
    const script = `
      const l = require(${JSON.stringify(cjsEntry)});
      l.open(${JSON.stringify(url)}).then((h) => {
        process.stdout.write("KIND=" + typeof h.embedderBackend + " TAG=" + h.embedderBackend());
      });
    `;
    const out = execFileSync(process.execPath, ["-e", script], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    });
    const m = /KIND=(\S+) TAG=(\S+)/.exec(out);
    expect(m, `CJS child produced no KIND=/TAG= line: ${out.slice(-400)}`).not.toBeNull();
    expect(m![1]).toBe("function");
    expect(KNOWN_TAGS).toContain(m![2]);
    expect(m![2]).not.toBe("unresolved");
  });
});

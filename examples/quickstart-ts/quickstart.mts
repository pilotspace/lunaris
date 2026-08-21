// Lunaris 10-minute quickstart — TypeScript.
//
// Mirrors examples/quickstart-rs/src/main.rs and
// examples/quickstart-py/quickstart.py against the same single-shard Moon.
// Demonstrates the v0.7 public TypeScript surface:
//
//     import { open, Scope, EpisodeBuilder } from "@pilotspace/lunaris";
//     const handle = await open("moon://127.0.0.1:6380");
//     const scoped = handle.scoped(Scope.new("quickstart")) as ScopedLunaris;
//     const lsn    = await scoped.ingest(new EpisodeBuilder("src", "..."));
//     const hits   = await scoped.recall("hello");
//
// Moon is the only backend as of 0.7.0. See docs/migration/0.6-to-0.7.md if
// you are coming from 0.6.x.
//
// ## Prerequisites
//
// 1. A single-shard Moon on moon://127.0.0.1:6380. `--shards 1` is REQUIRED:
//    Lunaris commits each ingest as one MULTI/EXEC transaction and a sharded
//    Moon refuses cross-shard writes. README.md has the runbook, and is honest
//    about why `docker pull` does not work yet.
// 2. npm install
// 3. The granite-r2 Q4_K_M GGUF staged at ~/.lunaris/models/.
// 4. export LUNARIS_STORE_URL="moon://127.0.0.1:6380"
// 5. npm start   (= npx tsx quickstart.mts)

// Named imports on purpose. These three ARE the public quickstart surface;
// binding them here means `tsc --noEmit` fails loudly the day one is renamed
// or dropped. That typecheck is one of the gates
// .github/workflows/examples.yml runs — see examples/README.md.
import { open, Scope, EpisodeBuilder } from "@pilotspace/lunaris";
import type { ScopedLunaris } from "@pilotspace/lunaris";

const SCOPE = "quickstart";
const SOURCE = "quickstart:demo";
const CONTENT = "# Hello from Lunaris\n\nThis is your first episode.";
const QUERY = "hello";

interface QuickstartHit {
  text: string;
  score: number;
}

async function main(): Promise<number> {
  // No default on purpose: guessing moon://127.0.0.1:6380 would let the
  // quickstart write demo episodes into whatever Moon happens to own that
  // port — which on a developer box is often a real store.
  const storeUrl = process.env.LUNARIS_STORE_URL;
  if (!storeUrl) {
    console.error(
      'error: set LUNARIS_STORE_URL="moon://127.0.0.1:6380" — see examples/quickstart-ts/README.md',
    );
    return 1;
  }

  console.log(`quickstart: opening lunaris handle at ${storeUrl}`);
  const handle = await open(storeUrl);

  // `Scope` is a validating newtype (^[A-Za-z0-9_\-.]{1,128}$); it throws
  // rather than letting an unvalidated string reach the keyspace.
  //
  // The cast is required because the ergonomic `LunarisHandle` declares
  // `scoped(scope: unknown): unknown` — the hand-written lunaris.d.ts layer
  // has not been given the generated ScopedLunaris type yet.
  const scoped = handle.scoped(Scope.new(SCOPE)) as ScopedLunaris;

  // The scope is stamped onto the episode by `ScopedLunaris.ingest`; the
  // builder carries no scope field, so a caller cannot smuggle one in.
  const lsn = await scoped.ingest(new EpisodeBuilder(SOURCE, CONTENT));
  console.log(`quickstart: ingested episode at lsn=${lsn} under scope \`${SCOPE}\``);

  // Recall through the scope-bound handle. Only hits from this scope's
  // partition come back; the retrieval root is Vector("chunks", 30).
  // `ScopedLunaris.recall` is declared as `Promise<any>` by the generated
  // binding, so narrow it here rather than propagating `any`.
  const hits = (await scoped.recall(QUERY)) as QuickstartHit[];
  console.log(`quickstart: recalled ${hits.length} hit(s) for "${QUERY}"`);
  const top = hits[0];
  if (top !== undefined) {
    // Trim the chunk body to one line so the demo output stays tidy.
    const snippet = top.text.slice(0, 60);
    console.log(`quickstart:   top hit score=${top.score.toFixed(2)} text="${snippet}"`);
  }

  // NOTE — there is deliberately no DSL section here. `scoped.dsl()` returns
  // the codegen-frozen native RetrievalBuilder, whose combinators throw and
  // which exposes no execute(). It *typechecks*, because lunaris.d.ts shadows
  // the name with the ergonomic class — so the compiler will not stop you.
  // The builder that works, `handle.recall().query(q).top(n).execute()`, has
  // no scope parameter and reads a different partition than this script wrote.
  // See README.md, "Where the DSL stops".
  return 0;
}

const code = await main();
process.exit(code);

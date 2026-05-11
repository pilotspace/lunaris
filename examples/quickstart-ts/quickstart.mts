// Lunaris 10-minute quickstart — TypeScript.
//
// Mirrors examples/quickstart-rs/src/main.rs and examples/quickstart-py/
// quickstart.py against the same Postgres backend. Demonstrates the
// v0.2.x public TypeScript surface:
//
//     import * as lunaris from "lunaris";
//     const handle  = await lunaris.open("postgres://...");
//     const lsn     = await handle.ingest(episode);
//     const hits    = await handle.recall().execute();
//
// ## Prerequisites
//
// 1. docker compose -f ../quickstart-rs/docker-compose.yml up -d
// 2. sqlx migrate run --source ../../crates/lunaris-storage-postgres/migrations
//        --database-url postgres://lunaris:lunaris@localhost:5432/lunaris
// 3. ollama serve & ; ollama pull nomic-embed-text
// 4. npm install lunaris   (or pnpm add lunaris)
// 5. export LUNARIS_PG_URL="postgres://lunaris:lunaris@localhost:5432/lunaris"
// 6. node --experimental-vm-modules quickstart.mts
//    (or: npx tsx quickstart.mts)

import * as lunaris from "lunaris";

function buildEpisode(scope: string, content: string): object {
  // Mirror the serde shape of lunaris_core::primitives::Episode.
  // The typed Scope + EpisodeBuilder TypeScript surface lands in v0.3.
  const ts = Date.now();
  const rand = Math.floor(Math.random() * 1_000_000_000)
    .toString(36)
    .padStart(8, "0");
  const id = `01${ts
    .toString(32)
    .toUpperCase()
    .padStart(10, "0")}${rand.toUpperCase().padStart(14, "0")}`.slice(0, 26);
  return {
    id,
    scope,
    source: "quickstart:demo",
    content,
    t_ref: null,
    bt: {
      valid: [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
      sys:   [{ wall_ms: 0, counter: 0, node_id: 0 }, null],
    },
    metadata: {},
  };
}

async function main(): Promise<number> {
  const pgUrl = process.env.LUNARIS_PG_URL;
  if (!pgUrl) {
    console.error("error: set LUNARIS_PG_URL — see examples/quickstart-ts/README.md");
    return 1;
  }

  console.log(`quickstart: opening lunaris handle at ${pgUrl}`);
  const handle = await lunaris.open(pgUrl);

  const scope = "quickstart";
  const episode = buildEpisode(scope, "# Hello from Lunaris\n\nFirst TS ingest.");
  const lsn = await handle.ingest(episode);
  console.log(`quickstart: ingested episode at lsn=${lsn} under scope \`${scope}\``);
  console.log("quickstart: ingest path verified; see README for recall walkthrough");
  return 0;
}

const code = await main();
process.exit(code);

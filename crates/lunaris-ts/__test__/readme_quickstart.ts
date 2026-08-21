// W0.6 — type-level guard for the README TypeScript quickstart.
//
// Everything below the BEGIN-README marker is the README's `ts` code block
// VERBATIM. `readme_quickstart.spec.mts` asserts byte equality with
// README.md, and `npm run typecheck` compiles this file against the shipped
// `index.d.ts` (via the `@pilotspace/lunaris` path mapping in
// tsconfig.readme.json). A runtime test alone cannot catch a wrong
// declaration file — this is the half that can.
//
// The `episode` binding is the README's narrative "same episode shape as
// Python" placeholder; it has no declaration in the block itself.
declare const episode: unknown;
export {};
// --- BEGIN-README ---
import { open, RetrievalBuilder } from "@pilotspace/lunaris";

const handle = await open("moon://127.0.0.1:6380");
const lsn = await handle.ingest(episode);           // same episode shape as Python
const hits = await new RetrievalBuilder()
  .bind(handle)
  .query("what does Alice like?")
  .top(5)
  .execute();

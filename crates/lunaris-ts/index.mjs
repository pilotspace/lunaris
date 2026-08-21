// Lunaris — ESM entry point for the `@pilotspace/lunaris` binding.
//
// This file holds NO logic. The whole ergonomic layer (Vector / Keyword /
// Graph / RetrievalBuilder / open / the pipeline-handle wrappers) lives in
// the CJS entry `lunaris.cjs`, and this module re-exports it name by name.
//
// That direction is deliberate. Before W0.6 the ergonomic layer lived HERE
// and the package's `main` was the raw napi-generated `index.js`, so ESM
// and CJS consumers got different classes off the same package:
// `require(…).RetrievalBuilder` had no `.bind()` / `.execute()` and the
// documented chain threw a TypeError. One implementation, re-exported,
// makes that divergence unrepresentable —
// `__test__/readme_quickstart.spec.mts` asserts the two entry points hand
// back the identical class objects.
//
// Named re-exports are written out rather than `export *` because Node's
// CJS named-export detection is static: an explicit list is the form that
// always resolves.

import lunaris from "./lunaris.cjs";

// ---- raw generated surface -------------------------------------------
export const Lunaris = lunaris.Lunaris;
export const GraphPipelineHandle = lunaris.GraphPipelineHandle;
export const ConsolidatorPipelineHandle = lunaris.ConsolidatorPipelineHandle;
export const openHandle = lunaris.openHandle;
export const recallSimpleExecute = lunaris.recallSimpleExecute;
export const conformanceFixtureEpisodes = lunaris.conformanceFixtureEpisodes;
export const scanKvPrefix = lunaris.scanKvPrefix;
export const getGraphPipeline = lunaris.getGraphPipeline;
export const getConsolidatorPipeline = lunaris.getConsolidatorPipeline;
export const graphPipelineEnable = lunaris.graphPipelineEnable;
export const graphPipelineDisable = lunaris.graphPipelineDisable;
export const graphPipelineIsEnabled = lunaris.graphPipelineIsEnabled;
export const consolidatorPipelineEnable = lunaris.consolidatorPipelineEnable;
export const consolidatorPipelineDisable = lunaris.consolidatorPipelineDisable;
export const consolidatorPipelineIsEnabled = lunaris.consolidatorPipelineIsEnabled;
export const fromEnv = lunaris.fromEnv;
export const fromEnvValue = lunaris.fromEnvValue;
export const fromConfig = lunaris.fromConfig;
export const __version__ = lunaris.__version__;

// Phase 21 Plan 21-01 — SDK custom embedder + reranker config.
export const EmbedderConfig = lunaris.EmbedderConfig;
export const RerankerConfig = lunaris.RerankerConfig;

// Wave 3G — v0.2 multi-agent partitioning surface (RFC 0001).
export const Scope = lunaris.Scope;
export const EpisodeBuilder = lunaris.EpisodeBuilder;
export const ScopedLunaris = lunaris.ScopedLunaris;
export const lunarisScoped = lunaris.lunarisScoped;

// Plan 10-03 + 11-03 — the Phase 10 conversational + Phase 11 documentary
// recipe wrappers, flat at the crate root. napi-rs 3.x's proc-macro registry
// surfaces every `#[napi]` class as a top-level identifier in the compiled
// `.node` (see 11-02b Known Limitations — `import { X } from
// 'lunaris/conversational'` is NOT supported).
export const ChatAgentMemory = lunaris.ChatAgentMemory;
export const MultiTurnConversation = lunaris.MultiTurnConversation;
export const SlackArchive = lunaris.SlackArchive;
export const SlackArchiveQuery = lunaris.SlackArchiveQuery;
export const EmailThreading = lunaris.EmailThreading;
export const MeetingNotesMemory = lunaris.MeetingNotesMemory;
export const MeetingNotesQuery = lunaris.MeetingNotesQuery;
export const DocumentKnowledgeBase = lunaris.DocumentKnowledgeBase;
export const ResearchPaperCorpus = lunaris.ResearchPaperCorpus;
export const CodeRepoMemory = lunaris.CodeRepoMemory;
export const TimelineReconstruction = lunaris.TimelineReconstruction;
export const CustomerSupportHistory = lunaris.CustomerSupportHistory;

export const LunarisError = lunaris.LunarisError;

// ---- ergonomic layer --------------------------------------------------
export const GraphPipelineHandleExt = lunaris.GraphPipelineHandleExt;
export const ConsolidatorPipelineHandleExt = lunaris.ConsolidatorPipelineHandleExt;
export const Vector = lunaris.Vector;
export const Keyword = lunaris.Keyword;
export const Graph = lunaris.Graph;
export const RetrievalBuilder = lunaris.RetrievalBuilder;
export const open = lunaris.open;

/** Internal: collapse an operator tree to the flat FFI plan. Exported for
 * the parity tests only; not part of the supported surface. */
export const _collapsePlan = lunaris._collapsePlan;

export default lunaris;

# lunaris-llm

Pluggable LLM-backend abstraction shared by the
[Lunaris](https://github.com/pilotspace/lunaris) extractor and verifier
pipelines.

The extractor / verifier LLM slots are remote-only: providers are selected
at runtime via `LUNARIS_EXTRACT_PROVIDER` / `LUNARIS_VERIFY_PROVIDER`
(`anthropic` | `openai` | `gemini` | `minimax` | `openai-compat`). This
crate defines the provider trait, request/response types, and the shared
retry / timeout plumbing those pipelines build on.

This crate is an internal building block of the `lunaris-memory` umbrella
crate; depend on the umbrella unless you are adding a provider.

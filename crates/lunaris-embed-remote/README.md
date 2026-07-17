# lunaris-embed-remote

Feature-gated remote-embedder escape hatch for
[Lunaris](https://github.com/pilotspace/lunaris).

Provides `OllamaEmbedder`, an HTTP client for an Ollama-compatible
`/api/embed` endpoint, for air-gapped or operator-managed deployments where
the in-process `lunaris-llamacpp` runtime cannot run. Enabled with
`--features embed-remote`; resolved only after the llama.cpp step in the
umbrella crate's embedder resolution order.

This crate is an internal building block of the `lunaris-memory` umbrella
crate; depend on the umbrella unless you are wiring the embedder directly.

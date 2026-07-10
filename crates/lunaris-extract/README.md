# lunaris-extract

LLM-backed entity, relation, and fact extractor for the [Lunaris](https://github.com/pilotspace/lunaris) agent
memory engine.

This crate is the entity/relation/fact extractor — remote-only since the llama.cpp cutover: Ollama and cloud-API providers (Anthropic / OpenAI / Gemini / MiniMax / any OpenAI-compatible URL) for hosted-LLM deployments.

## Use

```toml
[dependencies]
lunaris-extract = "0.2"
```

See the [Lunaris repository](https://github.com/pilotspace/lunaris) for
the umbrella crate, the 10-minute quickstart, the architecture overview,
and benchmarks.

## License

Apache-2.0. See [LICENSE](https://github.com/pilotspace/lunaris/blob/main/LICENSE).

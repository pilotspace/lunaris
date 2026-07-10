# lunaris-verify

Slow-path arbitration verifier for the [Lunaris](https://github.com/pilotspace/lunaris) agent
memory engine.

This crate is the slow-path verifier — remote-only since the llama.cpp cutover: Ollama and cloud-API arbitration backends (Anthropic / OpenAI / Gemini / MiniMax / any OpenAI-compatible URL) behind a typed Verifier trait.

## Use

```toml
[dependencies]
lunaris-verify = "0.2"
```

See the [Lunaris repository](https://github.com/pilotspace/lunaris) for
the umbrella crate, the 10-minute quickstart, the architecture overview,
and benchmarks.

## License

Apache-2.0. See [LICENSE](https://github.com/pilotspace/lunaris/blob/main/LICENSE).

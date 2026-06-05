# lunaris-verify

Slow-path arbitration verifier for the [Lunaris](https://github.com/pilotspace/lunaris) agent
memory engine.

This crate is the slow-path verifier — candle Gemma 3 27B (or 270M via the verify-small feature, RFC 0006), Ollama, and cloud-API arbitration backends behind a typed Verifier trait.

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

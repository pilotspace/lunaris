# lunaris-recipes

Composable, opinionated memory recipes for the [Lunaris](https://github.com/lunaris-dev/lunaris) agent
memory engine.

Where the umbrella `lunaris` crate gives you the raw engine, this crate layers ready-made recipes on top: four reusable primitives (`MessageStream`, `DocumentCorpus`, `TemporalQuery`, `WorkingMemory`) plus five conversational wrappers (chat, multi-turn, Slack, email, meeting notes) and five documentary wrappers (knowledge base, research papers, code repos, timelines, support history). Each wrapper is a thin, ≤30-LOC surface over an `Arc<lunaris::Lunaris>` with Moon + Postgres parity tests.

## Use

```toml
[dependencies]
lunaris-recipes = "0.2"
```

See the [Lunaris repository](https://github.com/lunaris-dev/lunaris) for
the umbrella crate, the Cookbook chapter with worked examples of every
recipe, and the architecture overview.

## License

Apache-2.0. See [LICENSE](https://github.com/lunaris-dev/lunaris/blob/main/LICENSE).

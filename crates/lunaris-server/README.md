# lunaris-server

HTTP + SSE memory server for the [Lunaris](https://github.com/lunaris-dev/lunaris) agent memory engine.

An [axum](https://docs.rs/axum)-based service that exposes a `lunaris::Lunaris` handle over MemoryProtocol 0.1: `POST /v1/{ingest,recall,forget}`, `GET /v1/snapshot/{lsn}`, plus unauthenticated `/healthz` and `/metrics`. Every `/v1/*` route is gated by a bearer token (mapped to a tenant scope) and a per-tenant rate limit; the tenant claim is the only source of truth for the partition scope. Configured entirely via CLI flags / environment variables.

## Use

```toml
[dependencies]
lunaris-server = "0.2"
```

See the [Lunaris repository](https://github.com/lunaris-dev/lunaris) for
the umbrella crate, the MemoryProtocol 0.1 spec, the Operations chapter
(full config reference), and the architecture overview.

## License

Apache-2.0. See [LICENSE](https://github.com/lunaris-dev/lunaris/blob/main/LICENSE).

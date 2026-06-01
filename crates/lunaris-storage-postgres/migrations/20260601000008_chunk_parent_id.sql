-- Migration 20260601000008 — STRUCT-03: additive parent_id column on chunks.
--
-- Adds a nullable BYTEA column `parent_id` to the `chunks` table.
--
-- Design notes:
--   - IF NOT EXISTS makes this migration idempotent (safe to re-run).
--   - BYTEA stores the raw 16-byte ULID bytes, matching how `id` and
--     `episode_id` are stored in the existing schema.
--   - NULL is the correct default: Phase 27 delivers the field + migration +
--     serde-compat; full parent wiring (setting non-NULL values via the
--     DocTree) lands in Phase 29.
--   - Moon backend: no DDL required — parent_id travels in the existing
--     JSONB KvPut payload as a nullable field.
--   - RLS: the existing `chunks_tenant_isolation` policy covers this column
--     transitively (it gates on `scope`, not individual columns).

ALTER TABLE chunks
    ADD COLUMN IF NOT EXISTS parent_id BYTEA NULL;

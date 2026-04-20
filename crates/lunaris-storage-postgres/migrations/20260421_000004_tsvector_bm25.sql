-- Phase 2 Plan 02-02 — Postgres BM25 / tsvector path for `KeywordPort::keyword_search`.
--
-- Adds a STORED, GENERATED `text_tsv tsvector` column to every table with text
-- payload AND a GIN index on each. Additive — does not touch any of the three
-- Phase 1 migration files. sqlx::migrate! runs files in lexicographic order, so
-- this migration applies after a clean migrate-up on existing dev databases.
--
-- The tsvector source per table:
--   chunks      → payload->>'text'
--   entities    → payload->>'name' || ' ' || payload->>'entity_type'
--   facts       → payload->>'fact_text'
--   communities → payload->>'summary'
--
-- All four use the `'english'` configuration. Queries use `plainto_tsquery('english', $1)`
-- so user input is parameterized and free of injection risk (T-02-02-01).
--
-- The `IF NOT EXISTS` guards make this migration idempotent — re-running on a
-- partially-applied DB does not error out.

-- ---------- chunks ----------
ALTER TABLE chunks
  ADD COLUMN IF NOT EXISTS text_tsv tsvector
    GENERATED ALWAYS AS (to_tsvector('english', coalesce(payload->>'text', ''))) STORED;
CREATE INDEX IF NOT EXISTS chunks_text_tsv_gin ON chunks USING gin (text_tsv);

-- ---------- entities ----------
ALTER TABLE entities
  ADD COLUMN IF NOT EXISTS text_tsv tsvector
    GENERATED ALWAYS AS (
      to_tsvector(
        'english',
        coalesce(payload->>'name', '') || ' ' || coalesce(payload->>'entity_type', '')
      )
    ) STORED;
CREATE INDEX IF NOT EXISTS entities_text_tsv_gin ON entities USING gin (text_tsv);

-- ---------- facts ----------
ALTER TABLE facts
  ADD COLUMN IF NOT EXISTS text_tsv tsvector
    GENERATED ALWAYS AS (to_tsvector('english', coalesce(payload->>'fact_text', ''))) STORED;
CREATE INDEX IF NOT EXISTS facts_text_tsv_gin ON facts USING gin (text_tsv);

-- ---------- communities ----------
ALTER TABLE communities
  ADD COLUMN IF NOT EXISTS text_tsv tsvector
    GENERATED ALWAYS AS (to_tsvector('english', coalesce(payload->>'summary', ''))) STORED;
CREATE INDEX IF NOT EXISTS communities_text_tsv_gin ON communities USING gin (text_tsv);

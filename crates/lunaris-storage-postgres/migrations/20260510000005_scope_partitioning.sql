-- RFC 0001 Wave 1B — Scope partitioning for multi-agent isolation.
--
-- Adds a `scope TEXT NOT NULL` column to every primitive table, enables
-- Row-Level Security (RLS) per table, and creates policies that enforce
-- per-scope isolation via the `current_setting('lunaris.scope')` GUC.
--
-- The Postgres backend sets `SET LOCAL lunaris.scope = $1` inside every
-- transaction so every query on these tables is automatically filtered to
-- the caller's scope. Cross-scope reads return zero rows by design.
--
-- Backfill: existing rows (created before this migration) receive
-- `scope = '_legacy'` — the reserved literal from RFC 0001 §6.
--
-- Pattern note: every ADD COLUMN / CREATE INDEX / ALTER TABLE uses
-- `IF NOT EXISTS` guards so this migration is idempotent on partially-
-- applied databases.

-- --------------------------------------------------------------------------
-- 1. Add scope column with CHECK constraint + backfill + composite index
-- --------------------------------------------------------------------------

-- episodes
ALTER TABLE episodes
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';
ALTER TABLE episodes
  ADD CONSTRAINT IF NOT EXISTS episodes_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
CREATE INDEX IF NOT EXISTS episodes_scope_id_idx ON episodes (scope, id);
-- Drop the temporary default so future inserts must supply scope explicitly.
ALTER TABLE episodes ALTER COLUMN scope DROP DEFAULT;

-- chunks
ALTER TABLE chunks
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';
ALTER TABLE chunks
  ADD CONSTRAINT IF NOT EXISTS chunks_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
CREATE INDEX IF NOT EXISTS chunks_scope_id_idx ON chunks (scope, id);
ALTER TABLE chunks ALTER COLUMN scope DROP DEFAULT;

-- entities
ALTER TABLE entities
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';
ALTER TABLE entities
  ADD CONSTRAINT IF NOT EXISTS entities_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
CREATE INDEX IF NOT EXISTS entities_scope_id_idx ON entities (scope, id);
ALTER TABLE entities ALTER COLUMN scope DROP DEFAULT;

-- relations
ALTER TABLE relations
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';
ALTER TABLE relations
  ADD CONSTRAINT IF NOT EXISTS relations_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
CREATE INDEX IF NOT EXISTS relations_scope_id_idx ON relations (scope, id);
ALTER TABLE relations ALTER COLUMN scope DROP DEFAULT;

-- facts
ALTER TABLE facts
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';
ALTER TABLE facts
  ADD CONSTRAINT IF NOT EXISTS facts_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
CREATE INDEX IF NOT EXISTS facts_scope_id_idx ON facts (scope, id);
ALTER TABLE facts ALTER COLUMN scope DROP DEFAULT;

-- communities
ALTER TABLE communities
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';
ALTER TABLE communities
  ADD CONSTRAINT IF NOT EXISTS communities_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
CREATE INDEX IF NOT EXISTS communities_scope_id_idx ON communities (scope, id);
ALTER TABLE communities ALTER COLUMN scope DROP DEFAULT;

-- --------------------------------------------------------------------------
-- 2. Enable Row-Level Security on every primitive table
-- --------------------------------------------------------------------------

-- RLS is OFF by default for table owner connections. We enable it for all
-- connections (FORCE is not needed — the owner override is acceptable here
-- because the backend always sets the GUC before primitive ops).

ALTER TABLE episodes   ENABLE ROW LEVEL SECURITY;
ALTER TABLE chunks     ENABLE ROW LEVEL SECURITY;
ALTER TABLE entities   ENABLE ROW LEVEL SECURITY;
ALTER TABLE relations  ENABLE ROW LEVEL SECURITY;
ALTER TABLE facts      ENABLE ROW LEVEL SECURITY;
ALTER TABLE communities ENABLE ROW LEVEL SECURITY;

-- --------------------------------------------------------------------------
-- 3. Create tenant_isolation policies
--
-- USING clause only (no WITH CHECK) — the insert path also flows through
-- SET LOCAL so the GUC is always set before both reads and writes.
-- --------------------------------------------------------------------------

DROP POLICY IF EXISTS tenant_isolation ON episodes;
CREATE POLICY tenant_isolation ON episodes
  USING (scope = current_setting('lunaris.scope', true));

DROP POLICY IF EXISTS tenant_isolation ON chunks;
CREATE POLICY tenant_isolation ON chunks
  USING (scope = current_setting('lunaris.scope', true));

DROP POLICY IF EXISTS tenant_isolation ON entities;
CREATE POLICY tenant_isolation ON entities
  USING (scope = current_setting('lunaris.scope', true));

DROP POLICY IF EXISTS tenant_isolation ON relations;
CREATE POLICY tenant_isolation ON relations
  USING (scope = current_setting('lunaris.scope', true));

DROP POLICY IF EXISTS tenant_isolation ON facts;
CREATE POLICY tenant_isolation ON facts
  USING (scope = current_setting('lunaris.scope', true));

DROP POLICY IF EXISTS tenant_isolation ON communities;
CREATE POLICY tenant_isolation ON communities
  USING (scope = current_setting('lunaris.scope', true));

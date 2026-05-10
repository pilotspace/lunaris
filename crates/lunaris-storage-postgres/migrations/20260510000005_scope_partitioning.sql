-- RFC 0001 Wave 1B — Scope partitioning for multi-agent isolation.
--
-- Adds a `scope TEXT NOT NULL` column to every primitive table AND lunaris_kv,
-- enables Row-Level Security (RLS) per table, and creates policies that enforce
-- per-scope isolation via the `current_setting('lunaris.scope', true)` GUC.
--
-- The Postgres backend sets `SET LOCAL lunaris.scope = $1` inside every
-- transaction so every query on these tables is automatically filtered to
-- the caller's scope. Cross-scope reads return zero rows by design.
--
-- Backfill: existing rows (created before this migration) receive
-- `scope = '_legacy'` — the reserved literal from RFC 0001 §6.
--
-- Idempotency notes:
--   ADD COLUMN IF NOT EXISTS    — supported, idempotent.
--   CREATE INDEX IF NOT EXISTS  — supported, idempotent.
--   ADD CONSTRAINT              — NOT supported with IF NOT EXISTS in Postgres.
--                                 We use a DO block to check pg_constraint
--                                 before adding the CHECK constraint, making
--                                 each constraint addition idempotent.
--   DROP POLICY IF EXISTS       — supported, idempotent.
--   CREATE POLICY               — idempotent via the preceding DROP.
--   ENABLE ROW LEVEL SECURITY   — idempotent (no-op if already enabled).

-- --------------------------------------------------------------------------
-- Helper: idempotent ADD CONSTRAINT via DO block
-- --------------------------------------------------------------------------
-- Each table uses the same pattern:
--   IF the named constraint does not already exist, add it.
-- This avoids the broken "ADD CONSTRAINT IF NOT EXISTS" syntax that Postgres
-- does not support.
-- --------------------------------------------------------------------------

-- --------------------------------------------------------------------------
-- 1. episodes
-- --------------------------------------------------------------------------
ALTER TABLE episodes
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';

DO $$ BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
    WHERE conname = 'episodes_scope_check' AND conrelid = 'episodes'::regclass
  ) THEN
    ALTER TABLE episodes
      ADD CONSTRAINT episodes_scope_check
        CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
  END IF;
END $$;

CREATE INDEX IF NOT EXISTS episodes_scope_id_idx ON episodes (scope, id);
ALTER TABLE episodes ALTER COLUMN scope DROP DEFAULT;

-- --------------------------------------------------------------------------
-- 2. chunks
-- --------------------------------------------------------------------------
ALTER TABLE chunks
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';

DO $$ BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
    WHERE conname = 'chunks_scope_check' AND conrelid = 'chunks'::regclass
  ) THEN
    ALTER TABLE chunks
      ADD CONSTRAINT chunks_scope_check
        CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
  END IF;
END $$;

CREATE INDEX IF NOT EXISTS chunks_scope_id_idx ON chunks (scope, id);
ALTER TABLE chunks ALTER COLUMN scope DROP DEFAULT;

-- --------------------------------------------------------------------------
-- 3. entities
-- --------------------------------------------------------------------------
ALTER TABLE entities
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';

DO $$ BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
    WHERE conname = 'entities_scope_check' AND conrelid = 'entities'::regclass
  ) THEN
    ALTER TABLE entities
      ADD CONSTRAINT entities_scope_check
        CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
  END IF;
END $$;

CREATE INDEX IF NOT EXISTS entities_scope_id_idx ON entities (scope, id);
ALTER TABLE entities ALTER COLUMN scope DROP DEFAULT;

-- --------------------------------------------------------------------------
-- 4. relations
-- --------------------------------------------------------------------------
ALTER TABLE relations
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';

DO $$ BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
    WHERE conname = 'relations_scope_check' AND conrelid = 'relations'::regclass
  ) THEN
    ALTER TABLE relations
      ADD CONSTRAINT relations_scope_check
        CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
  END IF;
END $$;

CREATE INDEX IF NOT EXISTS relations_scope_id_idx ON relations (scope, id);
ALTER TABLE relations ALTER COLUMN scope DROP DEFAULT;

-- --------------------------------------------------------------------------
-- 5. facts
-- --------------------------------------------------------------------------
ALTER TABLE facts
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';

DO $$ BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
    WHERE conname = 'facts_scope_check' AND conrelid = 'facts'::regclass
  ) THEN
    ALTER TABLE facts
      ADD CONSTRAINT facts_scope_check
        CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
  END IF;
END $$;

CREATE INDEX IF NOT EXISTS facts_scope_id_idx ON facts (scope, id);
ALTER TABLE facts ALTER COLUMN scope DROP DEFAULT;

-- --------------------------------------------------------------------------
-- 6. communities
-- --------------------------------------------------------------------------
ALTER TABLE communities
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';

DO $$ BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
    WHERE conname = 'communities_scope_check' AND conrelid = 'communities'::regclass
  ) THEN
    ALTER TABLE communities
      ADD CONSTRAINT communities_scope_check
        CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
  END IF;
END $$;

CREATE INDEX IF NOT EXISTS communities_scope_id_idx ON communities (scope, id);
ALTER TABLE communities ALTER COLUMN scope DROP DEFAULT;

-- --------------------------------------------------------------------------
-- 7. lunaris_kv — generic KV table used by KvPut / KvDelete / read_as_of /
--    scan_range. Must also be RLS-partitioned so KvPut-based writes are
--    isolated between scopes (RFC 0001 §3.5 closes the KvPut leak surface).
-- --------------------------------------------------------------------------
ALTER TABLE lunaris_kv
  ADD COLUMN IF NOT EXISTS scope TEXT NOT NULL DEFAULT '_legacy';

DO $$ BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
    WHERE conname = 'lunaris_kv_scope_check' AND conrelid = 'lunaris_kv'::regclass
  ) THEN
    ALTER TABLE lunaris_kv
      ADD CONSTRAINT lunaris_kv_scope_check
        CHECK (scope ~ '^[A-Za-z0-9_\-:.]{1,128}$');
  END IF;
END $$;

CREATE INDEX IF NOT EXISTS lunaris_kv_scope_key_idx ON lunaris_kv (scope, key);
ALTER TABLE lunaris_kv ALTER COLUMN scope DROP DEFAULT;

-- --------------------------------------------------------------------------
-- 8. Enable Row-Level Security on every primitive table + lunaris_kv
-- --------------------------------------------------------------------------

-- ENABLE ROW LEVEL SECURITY: activates RLS — idempotent (no-op if already on).
-- FORCE ROW LEVEL SECURITY: applies RLS even to the table owner role so
-- Postgres does not bypass policies when the connection user created the tables.
-- Without FORCE, the owner bypasses RLS, which would make the isolation tests
-- a false-pass when the backend connects as the same role that ran migrations.
ALTER TABLE episodes      ENABLE ROW LEVEL SECURITY;
ALTER TABLE episodes      FORCE  ROW LEVEL SECURITY;
ALTER TABLE chunks        ENABLE ROW LEVEL SECURITY;
ALTER TABLE chunks        FORCE  ROW LEVEL SECURITY;
ALTER TABLE entities      ENABLE ROW LEVEL SECURITY;
ALTER TABLE entities      FORCE  ROW LEVEL SECURITY;
ALTER TABLE relations     ENABLE ROW LEVEL SECURITY;
ALTER TABLE relations     FORCE  ROW LEVEL SECURITY;
ALTER TABLE facts         ENABLE ROW LEVEL SECURITY;
ALTER TABLE facts         FORCE  ROW LEVEL SECURITY;
ALTER TABLE communities   ENABLE ROW LEVEL SECURITY;
ALTER TABLE communities   FORCE  ROW LEVEL SECURITY;
ALTER TABLE lunaris_kv    ENABLE ROW LEVEL SECURITY;
ALTER TABLE lunaris_kv    FORCE  ROW LEVEL SECURITY;

-- --------------------------------------------------------------------------
-- 9. Create tenant_isolation policies (idempotent via DROP … IF EXISTS first)
--
-- USING clause only — WITH CHECK is omitted because the backend always sets
-- SET LOCAL before both writes and reads; the USING predicate covers both
-- directions under Postgres RLS semantics when only USING is specified.
-- current_setting('lunaris.scope', true) uses `missing_ok = true` so it
-- returns NULL (not an error) if the GUC is unset; the equality `scope = NULL`
-- evaluates to NULL (not TRUE), so unset-GUC sessions see zero rows — fail-safe.
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

DROP POLICY IF EXISTS tenant_isolation ON lunaris_kv;
CREATE POLICY tenant_isolation ON lunaris_kv
  USING (scope = current_setting('lunaris.scope', true));

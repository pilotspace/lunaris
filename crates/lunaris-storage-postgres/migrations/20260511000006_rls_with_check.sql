-- RC-3 (v0.2 release-gate review): add WITH CHECK to every tenant_isolation
-- policy so INSERT/UPDATE row scope must match the SET LOCAL GUC.
--
-- Background
-- ----------
-- Migration 20260510000005 created policies with `USING` only. Per the
-- Postgres docs (§5.8): when WITH CHECK is omitted, USING is used for
-- UPDATE row checks, but for INSERT only WITH CHECK is consulted. With
-- WITH CHECK also omitted, no row check happens on INSERT — a caller could
-- write `scope = 'evil'` while `SET LOCAL lunaris.scope = 'good'` and
-- Postgres would accept it.
--
-- Today's `atomic_write` path binds scope.as_str() to the column, so this
-- is defense-in-depth, not an exploitable bug. But the contract should be
-- enforceable from the database side too — otherwise a future caller (or
-- a serde-trusted wire scope, see RC-4) could mismatch the column from the
-- GUC and Postgres would silently accept.
--
-- This migration is idempotent — every CREATE POLICY is preceded by
-- DROP POLICY IF EXISTS, mirroring the original migration's pattern.

DROP POLICY IF EXISTS tenant_isolation ON episodes;
CREATE POLICY tenant_isolation ON episodes
  USING (scope = current_setting('lunaris.scope', true))
  WITH CHECK (scope = current_setting('lunaris.scope', true));

DROP POLICY IF EXISTS tenant_isolation ON chunks;
CREATE POLICY tenant_isolation ON chunks
  USING (scope = current_setting('lunaris.scope', true))
  WITH CHECK (scope = current_setting('lunaris.scope', true));

DROP POLICY IF EXISTS tenant_isolation ON entities;
CREATE POLICY tenant_isolation ON entities
  USING (scope = current_setting('lunaris.scope', true))
  WITH CHECK (scope = current_setting('lunaris.scope', true));

DROP POLICY IF EXISTS tenant_isolation ON relations;
CREATE POLICY tenant_isolation ON relations
  USING (scope = current_setting('lunaris.scope', true))
  WITH CHECK (scope = current_setting('lunaris.scope', true));

DROP POLICY IF EXISTS tenant_isolation ON facts;
CREATE POLICY tenant_isolation ON facts
  USING (scope = current_setting('lunaris.scope', true))
  WITH CHECK (scope = current_setting('lunaris.scope', true));

DROP POLICY IF EXISTS tenant_isolation ON communities;
CREATE POLICY tenant_isolation ON communities
  USING (scope = current_setting('lunaris.scope', true))
  WITH CHECK (scope = current_setting('lunaris.scope', true));

DROP POLICY IF EXISTS tenant_isolation ON lunaris_kv;
CREATE POLICY tenant_isolation ON lunaris_kv
  USING (scope = current_setting('lunaris.scope', true))
  WITH CHECK (scope = current_setting('lunaris.scope', true));

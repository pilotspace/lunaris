-- RC-2 (v0.2.1) — tighten the scope CHECK constraint to drop `:` from the
-- allowed alphabet.
--
-- Background
-- ----------
-- v0.2.0 shipped with the regex `^[A-Za-z0-9_\-:.]{1,128}$` on every
-- primitive table's `scope` column (migration 20260510000005). The `:` was
-- always a stylistic separator; nothing in Lunaris depended on its presence.
-- But the KV key format on Moon is `lunaris:{scope}:{kind}:{ulid}` — so a
-- scope ending in `:episode` byte-aliased `episode_prefix(&Scope("a"))` and
-- enabled SCAN-prefix bleed across scopes.
--
-- v0.2.1 closes this at the type level by tightening the Rust regex in
-- `lunaris_core::Scope::new` (drop `:`). This migration mirrors the change
-- on the Postgres side so the database CHECK constraint matches the Rust
-- regex byte-for-byte.
--
-- Compatibility
-- -------------
-- BREAKING for any v0.2.0 deployment that has stored scope strings
-- containing `:`. The recommended replacement is `.` or `-`. Before running
-- this migration, operators MUST rewrite any colon-containing rows:
--
--     UPDATE episodes      SET scope = replace(scope, ':', '.');
--     UPDATE chunks        SET scope = replace(scope, ':', '.');
--     UPDATE entities      SET scope = replace(scope, ':', '.');
--     UPDATE relations     SET scope = replace(scope, ':', '.');
--     UPDATE facts         SET scope = replace(scope, ':', '.');
--     UPDATE communities   SET scope = replace(scope, ':', '.');
--     UPDATE lunaris_kv    SET scope = replace(scope, ':', '.');
--
-- If any row still contains `:` at migration time, the `ALTER TABLE ...
-- ADD CHECK` step fails with constraint-violation per row; the migration
-- aborts cleanly and no schema change lands.
--
-- The Rust SDK's `Scope::new` post-v0.2.1 will reject the colon form at the
-- application boundary, so this migration brings the database CHECK back
-- into parity with the Rust contract.

ALTER TABLE episodes      DROP CONSTRAINT episodes_scope_check;
ALTER TABLE episodes      ADD  CONSTRAINT episodes_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-.]{1,128}$');

ALTER TABLE chunks        DROP CONSTRAINT chunks_scope_check;
ALTER TABLE chunks        ADD  CONSTRAINT chunks_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-.]{1,128}$');

ALTER TABLE entities      DROP CONSTRAINT entities_scope_check;
ALTER TABLE entities      ADD  CONSTRAINT entities_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-.]{1,128}$');

ALTER TABLE relations     DROP CONSTRAINT relations_scope_check;
ALTER TABLE relations     ADD  CONSTRAINT relations_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-.]{1,128}$');

ALTER TABLE facts         DROP CONSTRAINT facts_scope_check;
ALTER TABLE facts         ADD  CONSTRAINT facts_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-.]{1,128}$');

ALTER TABLE communities   DROP CONSTRAINT communities_scope_check;
ALTER TABLE communities   ADD  CONSTRAINT communities_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-.]{1,128}$');

ALTER TABLE lunaris_kv    DROP CONSTRAINT lunaris_kv_scope_check;
ALTER TABLE lunaris_kv    ADD  CONSTRAINT lunaris_kv_scope_check
    CHECK (scope ~ '^[A-Za-z0-9_\-.]{1,128}$');

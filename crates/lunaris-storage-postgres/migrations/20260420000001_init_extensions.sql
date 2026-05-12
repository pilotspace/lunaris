-- Extensions used by Lunaris on Postgres.
--
-- pgvector and pgmq are hard requirements (vector search + the
-- consolidate/verify/audit queues). Apache AGE is OPT-IN: the graph retrieval
-- operators need it, but a stock / managed Postgres (RDS, Cloud SQL, Supabase,
-- Neon) ships pgvector — and often pgmq — yet not AGE. So the AGE setup is
-- best-effort: if the `age` binary isn't installed the migration still
-- succeeds, `graph_traverse()` simply errors at call time, and an operator who
-- wants the graph runs `CREATE EXTENSION age;` later (then restarts so
-- `LOAD 'age'` takes effect per session — see `PgClient::connect`).
--
-- NOTE: this file's contents changed in v0.2.x (the AGE lines moved into the
-- DO block below). sqlx tracks migrations by checksum, so a database that
-- already applied the pre-v0.2.x version will report a checksum mismatch on
-- the next `migrate` — re-apply the migration set on a fresh schema (the
-- 0.1→0.2 upgrade already requires this).

CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pgmq;

DO $age$
BEGIN
    EXECUTE 'CREATE EXTENSION IF NOT EXISTS age';
    EXECUTE 'LOAD ''age''';
    PERFORM set_config('search_path', 'ag_catalog, "$user", public', true);
EXCEPTION WHEN OTHERS THEN
    RAISE NOTICE 'Apache AGE unavailable (%); Lunaris graph operators are disabled until `CREATE EXTENSION age` is run by a superuser', SQLERRM;
END
$age$;

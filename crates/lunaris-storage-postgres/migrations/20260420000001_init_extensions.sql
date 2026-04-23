-- Extensions used by Lunaris on Postgres.
-- pgvector + AGE + pgmq are required. Apache AGE additionally needs `LOAD 'age'` per session
-- before its functions are visible; we re-issue it from PgClient::connect() via SET search_path.

CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS age;
LOAD 'age';
SET LOCAL search_path = ag_catalog, "$user", public;
CREATE EXTENSION IF NOT EXISTS pgmq;

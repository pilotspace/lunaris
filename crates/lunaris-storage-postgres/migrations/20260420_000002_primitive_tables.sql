-- Six bi-temporal tables — one per primitive. Naming + columns match the keyspace
-- convention from Moon (lunaris-storage-moon/src/keyspace.rs) and the field set
-- in lunaris-core/src/primitives.rs.
--
-- Bi-temporal columns:
--   valid_from / valid_to : "when the fact is true in the world"
--   sys_from   / sys_to   : "when the system observed/recorded the fact"
--   *_hlc                 : packed HLC (BIGINT, wall_ms<<32 | counter) for byte-identical AS_OF parity vs Moon

-- ---------- episodes ----------
CREATE TABLE IF NOT EXISTS episodes (
  id              BYTEA PRIMARY KEY,                       -- ulid bytes (16) — also stored in payload.id
  payload         JSONB NOT NULL,
  valid_from      TIMESTAMPTZ NOT NULL,
  valid_to        TIMESTAMPTZ NULL,
  sys_from        TIMESTAMPTZ NOT NULL,
  sys_to          TIMESTAMPTZ NULL,
  valid_from_hlc  BIGINT NOT NULL,
  sys_from_hlc    BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS episodes_valid_idx ON episodes USING gist (tstzrange(valid_from, valid_to));
CREATE INDEX IF NOT EXISTS episodes_sys_idx   ON episodes USING gist (tstzrange(sys_from,   sys_to));

-- ---------- chunks ----------
CREATE TABLE IF NOT EXISTS chunks (
  id              BYTEA PRIMARY KEY,
  payload         JSONB NOT NULL,
  embedding       vector(768) NULL,                        -- 768d EmbeddingGemma default
  valid_from      TIMESTAMPTZ NOT NULL,
  valid_to        TIMESTAMPTZ NULL,
  sys_from        TIMESTAMPTZ NOT NULL,
  sys_to          TIMESTAMPTZ NULL,
  valid_from_hlc  BIGINT NOT NULL,
  sys_from_hlc    BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS chunks_valid_idx     ON chunks USING gist (tstzrange(valid_from, valid_to));
CREATE INDEX IF NOT EXISTS chunks_embedding_idx ON chunks USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);

-- ---------- entities ----------
CREATE TABLE IF NOT EXISTS entities (
  id              BYTEA PRIMARY KEY,
  payload         JSONB NOT NULL,
  embedding       vector(768) NULL,
  valid_from      TIMESTAMPTZ NOT NULL,
  valid_to        TIMESTAMPTZ NULL,
  sys_from        TIMESTAMPTZ NOT NULL,
  sys_to          TIMESTAMPTZ NULL,
  valid_from_hlc  BIGINT NOT NULL,
  sys_from_hlc    BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS entities_valid_idx     ON entities USING gist (tstzrange(valid_from, valid_to));
CREATE INDEX IF NOT EXISTS entities_embedding_idx ON entities USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);

-- ---------- relations ----------
CREATE TABLE IF NOT EXISTS relations (
  id              BYTEA PRIMARY KEY,
  src             BYTEA NOT NULL,
  dst             BYTEA NOT NULL,
  payload         JSONB NOT NULL,
  valid_from      TIMESTAMPTZ NOT NULL,
  valid_to        TIMESTAMPTZ NULL,
  sys_from        TIMESTAMPTZ NOT NULL,
  sys_to          TIMESTAMPTZ NULL,
  valid_from_hlc  BIGINT NOT NULL,
  sys_from_hlc    BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS relations_src_idx   ON relations (src);
CREATE INDEX IF NOT EXISTS relations_dst_idx   ON relations (dst);
CREATE INDEX IF NOT EXISTS relations_valid_idx ON relations USING gist (tstzrange(valid_from, valid_to));

-- ---------- facts ----------
CREATE TABLE IF NOT EXISTS facts (
  id              BYTEA PRIMARY KEY,
  payload         JSONB NOT NULL,
  embedding       vector(768) NULL,
  valid_from      TIMESTAMPTZ NOT NULL,
  valid_to        TIMESTAMPTZ NULL,
  sys_from        TIMESTAMPTZ NOT NULL,
  sys_to          TIMESTAMPTZ NULL,
  valid_from_hlc  BIGINT NOT NULL,
  sys_from_hlc    BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS facts_valid_idx     ON facts USING gist (tstzrange(valid_from, valid_to));
CREATE INDEX IF NOT EXISTS facts_embedding_idx ON facts USING ivfflat (embedding vector_cosine_ops) WITH (lists = 100);

-- ---------- communities ----------
CREATE TABLE IF NOT EXISTS communities (
  id                 BYTEA PRIMARY KEY,
  payload            JSONB NOT NULL,
  summary_embedding  vector(768) NULL,
  valid_from         TIMESTAMPTZ NOT NULL,
  valid_to           TIMESTAMPTZ NULL,
  sys_from           TIMESTAMPTZ NOT NULL,
  sys_to             TIMESTAMPTZ NULL,
  valid_from_hlc     BIGINT NOT NULL,
  sys_from_hlc       BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS communities_valid_idx     ON communities USING gist (tstzrange(valid_from, valid_to));
CREATE INDEX IF NOT EXISTS communities_embedding_idx ON communities USING ivfflat (summary_embedding vector_cosine_ops) WITH (lists = 100);

-- ---------- generic kv (used by atomic_write WriteOp::KvPut for raw key/value rows) ----------
CREATE TABLE IF NOT EXISTS lunaris_kv (
  key             BYTEA PRIMARY KEY,
  value           BYTEA NOT NULL,
  bt              JSONB NOT NULL,                          -- serialized BiTemporal value
  valid_from      TIMESTAMPTZ NOT NULL,
  valid_to        TIMESTAMPTZ NULL,
  sys_from        TIMESTAMPTZ NOT NULL,
  sys_to          TIMESTAMPTZ NULL,
  valid_from_hlc  BIGINT NOT NULL,
  sys_from_hlc    BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS lunaris_kv_valid_idx ON lunaris_kv USING gist (tstzrange(valid_from, valid_to));

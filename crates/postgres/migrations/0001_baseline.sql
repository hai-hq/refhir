-- Baseline schema, version 1: the tables, indexes and columns of the last release that
-- created its schema at startup with no versions.
--
-- FROZEN. Its checksum is recorded in every database that applied it, and a database whose
-- recorded checksum differs from this file's refuses to start. Never edit it: add a new
-- numbered migration instead.
--
-- Everything here is one transaction, so `indexes_by_ts` is a plain CREATE INDEX (Postgres
-- refuses CONCURRENTLY inside a transaction). On a new database the table is empty and that is
-- instant.

CREATE TABLE IF NOT EXISTS documents (
    id BYTEA NOT NULL,
    ts BIGINT NOT NULL,
    table_id BYTEA NOT NULL,
    json_value TEXT NULL,
    deleted INTEGER NOT NULL,
    prev_ts BIGINT,
    PRIMARY KEY (ts, table_id, id)
);
CREATE INDEX IF NOT EXISTS documents_by_table_and_id ON documents (table_id, id, ts);

CREATE TABLE IF NOT EXISTS indexes (
    index_id BYTEA NOT NULL,
    ts BIGINT NOT NULL,
    key BYTEA NOT NULL,
    deleted INTEGER NOT NULL,
    table_id BYTEA NULL,
    document_id BYTEA NULL,
    PRIMARY KEY (index_id, key, ts)
);
-- A reader in another process learns what a commit wrote by scanning index rows by ts.
CREATE INDEX IF NOT EXISTS indexes_by_ts ON indexes (ts);

CREATE TABLE IF NOT EXISTS persistence_globals (
    key TEXT NOT NULL,
    json_value TEXT NOT NULL,
    PRIMARY KEY (key)
);

CREATE TABLE IF NOT EXISTS packages (
    id BIGSERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    fhir_version TEXT NOT NULL,
    UNIQUE (name, version)
);
CREATE TABLE IF NOT EXISTS structure_definitions (
    package_id BIGINT NOT NULL,
    url TEXT NOT NULL,
    type_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    is_abstract INTEGER NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);
CREATE TABLE IF NOT EXISTS search_parameters (
    package_id BIGINT NOT NULL,
    url TEXT NOT NULL,
    base TEXT NOT NULL,
    code TEXT NOT NULL,
    param_type TEXT NOT NULL,
    expression TEXT,
    status TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url, base)
);
-- Installing a parameter looks up the rows that share its base and code.
CREATE INDEX IF NOT EXISTS search_parameters_by_base_code ON search_parameters (base, code);
CREATE TABLE IF NOT EXISTS index_backfills (
    id BIGSERIAL PRIMARY KEY,
    package_id BIGINT NOT NULL,
    watermark_ts BIGINT NOT NULL,
    tablet TEXT NOT NULL,
    cursor_id TEXT,
    done INTEGER NOT NULL,
    claimed_epoch BIGINT
);
-- Databases created before backfills were claimed by a writer epoch.
ALTER TABLE index_backfills ADD COLUMN IF NOT EXISTS claimed_epoch BIGINT;
CREATE TABLE IF NOT EXISTS terminology (
    package_id BIGINT NOT NULL,
    resource_type TEXT NOT NULL,
    url TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);
CREATE TABLE IF NOT EXISTS value_set_expansion (
    package_id BIGINT NOT NULL,
    url TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);
CREATE TABLE IF NOT EXISTS backfill_parameter (
    backfill_id BIGINT NOT NULL,
    url TEXT NOT NULL,
    PRIMARY KEY (backfill_id, url)
);

CREATE TABLE IF NOT EXISTS smart_oauth (
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    json TEXT NOT NULL,
    exp BIGINT NOT NULL,
    PRIMARY KEY (kind, id)
);
CREATE INDEX IF NOT EXISTS smart_oauth_by_exp ON smart_oauth (exp);

CREATE TABLE IF NOT EXISTS jobs (
    id TEXT NOT NULL PRIMARY KEY,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    request TEXT NOT NULL,
    result TEXT,
    error TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    completed_at BIGINT,
    lease_owner TEXT,
    lease_until BIGINT
);
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS lease_owner TEXT;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS lease_until BIGINT;
CREATE INDEX IF NOT EXISTS jobs_by_status ON jobs (status, created_at);

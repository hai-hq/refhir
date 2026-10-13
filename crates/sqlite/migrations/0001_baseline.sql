-- Baseline schema, version 1: the tables, indexes and columns of the last release that
-- created its schema at startup with no versions.
--
-- FROZEN. Its checksum is recorded in every database that applied it, and a database whose
-- recorded checksum differs from this file's refuses to start. Never edit it: add a new
-- numbered migration instead.

CREATE TABLE IF NOT EXISTS documents (
    id BLOB NOT NULL,
    ts INTEGER NOT NULL,
    table_id BLOB NOT NULL,
    json_value TEXT NULL,
    deleted INTEGER NOT NULL,
    prev_ts INTEGER,
    PRIMARY KEY (ts, table_id, id)
);
CREATE INDEX IF NOT EXISTS documents_by_table_and_id ON documents (table_id, id, ts);

CREATE TABLE IF NOT EXISTS indexes (
    index_id BLOB NOT NULL,
    ts INTEGER NOT NULL,
    key BLOB NOT NULL,
    deleted INTEGER NOT NULL,
    table_id BLOB NULL,
    document_id BLOB NULL,
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
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    fhir_version TEXT NOT NULL,
    UNIQUE (name, version)
);
CREATE TABLE IF NOT EXISTS structure_definitions (
    package_id INTEGER NOT NULL,
    url TEXT NOT NULL,
    type_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    is_abstract INTEGER NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);
CREATE TABLE IF NOT EXISTS search_parameters (
    package_id INTEGER NOT NULL,
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
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    package_id INTEGER NOT NULL,
    watermark_ts INTEGER NOT NULL,
    tablet TEXT NOT NULL,
    cursor_id TEXT,
    done INTEGER NOT NULL,
    claimed_epoch INTEGER
);
CREATE TABLE IF NOT EXISTS terminology (
    package_id INTEGER NOT NULL,
    resource_type TEXT NOT NULL,
    url TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);
CREATE TABLE IF NOT EXISTS value_set_expansion (
    package_id INTEGER NOT NULL,
    url TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);
CREATE TABLE IF NOT EXISTS backfill_parameter (
    backfill_id INTEGER NOT NULL,
    url TEXT NOT NULL,
    PRIMARY KEY (backfill_id, url)
);

CREATE TABLE IF NOT EXISTS smart_oauth (
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    json TEXT NOT NULL,
    exp INTEGER NOT NULL,
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
    created_at INTEGER NOT NULL,
    completed_at INTEGER,
    lease_owner TEXT,
    lease_until INTEGER
);
CREATE INDEX IF NOT EXISTS jobs_by_status ON jobs (status, created_at);

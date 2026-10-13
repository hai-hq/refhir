-- Version 2: what the server records about its own traffic. Usage counters per caller, day and
-- class; a bounded log of recent requests; the audit trail, hash-chained (`prev_hash`, `hash`);
-- and the heartbeat by which servers sharing the database count one another.
--
-- FROZEN once released: add a new numbered migration instead of editing this file.

CREATE TABLE usage_daily (
    period_day INTEGER NOT NULL,
    principal TEXT NOT NULL,
    request_class TEXT NOT NULL,
    requests INTEGER NOT NULL DEFAULT 0,
    client_errors INTEGER NOT NULL DEFAULT 0,
    server_errors INTEGER NOT NULL DEFAULT 0,
    throttled INTEGER NOT NULL DEFAULT 0,
    bytes_in INTEGER NOT NULL DEFAULT 0,
    bytes_out INTEGER NOT NULL DEFAULT 0,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (period_day, principal, request_class)
);
CREATE INDEX usage_daily_by_principal ON usage_daily (principal, period_day);
CREATE TABLE request_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    request_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    remote_ip TEXT,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    params TEXT NOT NULL,
    request_class TEXT NOT NULL,
    status INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    bytes_in INTEGER NOT NULL,
    bytes_out INTEGER NOT NULL
);
CREATE INDEX request_log_by_ts ON request_log (ts);
CREATE INDEX request_log_by_principal ON request_log (principal, id);
CREATE INDEX request_log_by_request ON request_log (request_id);
CREATE TABLE governance_members (
    owner TEXT NOT NULL PRIMARY KEY,
    seen_ms INTEGER NOT NULL
);
CREATE TABLE audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    request_id TEXT NOT NULL,
    source TEXT NOT NULL,
    action TEXT NOT NULL,
    subtype TEXT NOT NULL,
    outcome INTEGER NOT NULL,
    status INTEGER NOT NULL,
    principal TEXT NOT NULL,
    client_id TEXT,
    user_id TEXT,
    fhir_user TEXT,
    remote_ip TEXT,
    resource_type TEXT,
    resource_id TEXT,
    patient TEXT,
    query TEXT,
    detail TEXT,
    prev_hash BLOB,
    hash BLOB
);
CREATE INDEX audit_log_by_ts ON audit_log (ts);
CREATE INDEX audit_log_by_resource ON audit_log (resource_type, resource_id, id);
CREATE INDEX audit_log_by_patient ON audit_log (patient, id);
CREATE INDEX audit_log_by_principal ON audit_log (principal, id);
CREATE INDEX audit_log_by_request ON audit_log (request_id);

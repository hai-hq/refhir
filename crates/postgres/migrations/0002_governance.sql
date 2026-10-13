-- Version 2: what the server records about its own traffic. Usage counters per caller, day and
-- class; a bounded log of recent requests; the audit trail, hash-chained (`prev_hash`, `hash`);
-- and the heartbeat by which servers sharing the database count one another.
--
-- Every number is BIGINT so that it reads back as an i64.
--
-- FROZEN once released: add a new numbered migration instead of editing this file.

CREATE TABLE usage_daily (
    period_day BIGINT NOT NULL,
    principal TEXT NOT NULL,
    request_class TEXT NOT NULL,
    requests BIGINT NOT NULL DEFAULT 0,
    client_errors BIGINT NOT NULL DEFAULT 0,
    server_errors BIGINT NOT NULL DEFAULT 0,
    throttled BIGINT NOT NULL DEFAULT 0,
    bytes_in BIGINT NOT NULL DEFAULT 0,
    bytes_out BIGINT NOT NULL DEFAULT 0,
    duration_ms BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (period_day, principal, request_class)
);
CREATE INDEX usage_daily_by_principal ON usage_daily (principal, period_day);
CREATE TABLE request_log (
    id BIGSERIAL PRIMARY KEY,
    ts BIGINT NOT NULL,
    request_id TEXT NOT NULL,
    principal TEXT NOT NULL,
    remote_ip TEXT,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    params TEXT NOT NULL,
    request_class TEXT NOT NULL,
    status BIGINT NOT NULL,
    duration_ms BIGINT NOT NULL,
    bytes_in BIGINT NOT NULL,
    bytes_out BIGINT NOT NULL
);
CREATE INDEX request_log_by_ts ON request_log (ts);
CREATE INDEX request_log_by_principal ON request_log (principal, id);
CREATE INDEX request_log_by_request ON request_log (request_id);
CREATE TABLE governance_members (
    owner TEXT NOT NULL PRIMARY KEY,
    seen_ms BIGINT NOT NULL
);
CREATE TABLE audit_log (
    id BIGSERIAL PRIMARY KEY,
    ts BIGINT NOT NULL,
    request_id TEXT NOT NULL,
    source TEXT NOT NULL,
    action TEXT NOT NULL,
    subtype TEXT NOT NULL,
    outcome BIGINT NOT NULL,
    status BIGINT NOT NULL,
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
    prev_hash BYTEA,
    hash BYTEA
);
CREATE INDEX audit_log_by_ts ON audit_log (ts);
CREATE INDEX audit_log_by_resource ON audit_log (resource_type, resource_id, id);
CREATE INDEX audit_log_by_patient ON audit_log (patient, id);
CREATE INDEX audit_log_by_principal ON audit_log (principal, id);
CREATE INDEX audit_log_by_request ON audit_log (request_id);

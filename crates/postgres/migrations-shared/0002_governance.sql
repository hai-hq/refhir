-- Shared-layout version 2: what the server records about its own traffic, for a database that
-- many tenants share. The classic `0002_governance.sql` with `instance_id` leading every key and
-- index. Usage counters per caller, day and class; a bounded log of recent requests; the audit
-- trail, hash-chained (`prev_hash`, `hash`) separately for each tenant; and the heartbeat by which
-- a tenant's servers count one another.
--
-- Every number is BIGINT so that it reads back as an i64.
--
-- FROZEN once released: add a new numbered migration instead of editing this file.

CREATE TABLE usage_daily (
    instance_id INTEGER NOT NULL,
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
    PRIMARY KEY (instance_id, period_day, principal, request_class)
);
CREATE INDEX usage_daily_by_principal ON usage_daily (instance_id, principal, period_day);
CREATE TABLE request_log (
    id BIGSERIAL PRIMARY KEY,
    instance_id INTEGER NOT NULL,
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
CREATE INDEX request_log_by_ts ON request_log (instance_id, ts);
CREATE INDEX request_log_by_principal ON request_log (instance_id, principal, id);
CREATE INDEX request_log_by_request ON request_log (instance_id, request_id);
-- The newest rows of one tenant (`ORDER BY id DESC LIMIT n`) read this, not the whole table.
CREATE INDEX request_log_by_instance ON request_log (instance_id, id);
CREATE TABLE governance_members (
    instance_id INTEGER NOT NULL,
    owner TEXT NOT NULL,
    seen_ms BIGINT NOT NULL,
    PRIMARY KEY (instance_id, owner)
);
CREATE TABLE audit_log (
    id BIGSERIAL PRIMARY KEY,
    instance_id INTEGER NOT NULL,
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
CREATE INDEX audit_log_by_ts ON audit_log (instance_id, ts);
CREATE INDEX audit_log_by_resource ON audit_log (instance_id, resource_type, resource_id, id);
CREATE INDEX audit_log_by_patient ON audit_log (instance_id, patient, id);
CREATE INDEX audit_log_by_principal ON audit_log (instance_id, principal, id);
CREATE INDEX audit_log_by_request ON audit_log (instance_id, request_id);
-- The chain tip and the newest rows of one tenant (`ORDER BY id DESC LIMIT n`).
CREATE INDEX audit_log_by_instance ON audit_log (instance_id, id);

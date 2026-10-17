-- Shared-layout baseline, version 1: the tables of the classic baseline (`../migrations/`) for a
-- database that many tenants share. Every table has an `instance_id` that leads its primary key,
-- its unique constraints and its indexes, so a tenant's rows are one contiguous range of each
-- one, and a registry, `refhir_tenants`, hands the ids out.
--
-- This stream is only ever applied to an empty database (`refhir tenant create`), so it creates
-- tables plainly: no `IF NOT EXISTS`, and none of the `ADD COLUMN` lines the classic baseline
-- keeps for databases made by an older release. A test applies both streams and checks that this
-- one is the classic schema plus the tenant column (and the few additions it lists): a change to
-- one stream needs the same change here.
--
-- FROZEN once released. Its checksum is recorded in every database that applied it, and a
-- database whose recorded checksum differs from this file's refuses to start. Never edit it:
-- add a new numbered migration instead.

-- Which tenants a database holds. Id 0 is reserved for what every tenant shares.
CREATE TABLE refhir_tenants (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT refhir_tenants_name CHECK (id = 0 OR name ~ '^[a-z0-9][a-z0-9-]{0,62}$')
);
INSERT INTO refhir_tenants (id, name) VALUES (0, '_base');

CREATE TABLE documents (
    instance_id INTEGER NOT NULL,
    id BYTEA NOT NULL,
    ts BIGINT NOT NULL,
    table_id BYTEA NOT NULL,
    json_value TEXT NULL,
    deleted INTEGER NOT NULL,
    prev_ts BIGINT,
    PRIMARY KEY (instance_id, ts, table_id, id)
);
CREATE INDEX documents_by_table_and_id ON documents (instance_id, table_id, id, ts);

CREATE TABLE indexes (
    instance_id INTEGER NOT NULL,
    index_id BYTEA NOT NULL,
    ts BIGINT NOT NULL,
    key BYTEA NOT NULL,
    deleted INTEGER NOT NULL,
    table_id BYTEA NULL,
    document_id BYTEA NULL,
    PRIMARY KEY (instance_id, index_id, key, ts)
);
-- A reader in another process learns what a commit wrote by scanning index rows by ts.
CREATE INDEX indexes_by_ts ON indexes (instance_id, ts);

CREATE TABLE persistence_globals (
    instance_id INTEGER NOT NULL,
    key TEXT NOT NULL,
    json_value TEXT NOT NULL,
    PRIMARY KEY (instance_id, key)
);

CREATE TABLE packages (
    id BIGSERIAL PRIMARY KEY,
    -- 0 for a package every tenant shares, or the tenant that holds it alone.
    instance_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    fhir_version TEXT NOT NULL,
    -- For a shared package: a digest of the source it was made from. A tenant is given the shared
    -- copy only when it asks for the same source.
    digest TEXT,
    UNIQUE (instance_id, name, version)
);
-- The packages a tenant has, in the order it got them. The order matters: the catalog settles a
-- clash between two definitions by it, and a package shared by every tenant has one id for all of
-- them, so the order is this table's `seq` and not the package id. `owner` is whose rows hold the
-- package's definitions and terminology: 0 for a package every tenant shares, or the tenant itself.
CREATE TABLE tenant_packages (
    instance_id INTEGER NOT NULL,
    package_id BIGINT NOT NULL REFERENCES packages (id),
    owner INTEGER NOT NULL,
    seq BIGSERIAL NOT NULL,
    PRIMARY KEY (instance_id, package_id),
    UNIQUE (instance_id, seq)
);
CREATE TABLE structure_definitions (
    instance_id INTEGER NOT NULL,
    package_id BIGINT NOT NULL,
    url TEXT NOT NULL,
    type_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    is_abstract INTEGER NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (instance_id, package_id, url)
);
CREATE TABLE search_parameters (
    instance_id INTEGER NOT NULL,
    package_id BIGINT NOT NULL,
    url TEXT NOT NULL,
    base TEXT NOT NULL,
    code TEXT NOT NULL,
    param_type TEXT NOT NULL,
    expression TEXT,
    status TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (instance_id, package_id, url, base)
);
-- Installing a parameter looks up the rows that share its base and code.
CREATE INDEX search_parameters_by_base_code ON search_parameters (instance_id, base, code);
CREATE TABLE index_backfills (
    id BIGSERIAL PRIMARY KEY,
    instance_id INTEGER NOT NULL,
    package_id BIGINT NOT NULL,
    watermark_ts BIGINT NOT NULL,
    tablet TEXT NOT NULL,
    cursor_id TEXT,
    done INTEGER NOT NULL,
    claimed_epoch BIGINT
);
CREATE INDEX index_backfills_by_instance ON index_backfills (instance_id, id);
CREATE TABLE terminology (
    instance_id INTEGER NOT NULL,
    package_id BIGINT NOT NULL,
    resource_type TEXT NOT NULL,
    url TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (instance_id, package_id, url)
);
-- A write checks one URL, a bare one also matching its versions (`url|version`).
CREATE INDEX terminology_by_url ON terminology (instance_id, url);
CREATE INDEX terminology_by_bare_url ON terminology (instance_id, (split_part(url, '|', 1)));
CREATE TABLE value_set_expansion (
    instance_id INTEGER NOT NULL,
    package_id BIGINT NOT NULL,
    url TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (instance_id, package_id, url)
);
CREATE TABLE backfill_parameter (
    instance_id INTEGER NOT NULL,
    backfill_id BIGINT NOT NULL,
    url TEXT NOT NULL,
    PRIMARY KEY (instance_id, backfill_id, url)
);

CREATE TABLE smart_oauth (
    instance_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    json TEXT NOT NULL,
    exp BIGINT NOT NULL,
    PRIMARY KEY (instance_id, kind, id)
);
CREATE INDEX smart_oauth_by_exp ON smart_oauth (instance_id, exp);

CREATE TABLE jobs (
    instance_id INTEGER NOT NULL,
    id TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    request TEXT NOT NULL,
    result TEXT,
    error TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    completed_at BIGINT,
    lease_owner TEXT,
    lease_until BIGINT,
    PRIMARY KEY (instance_id, id)
);
CREATE INDEX jobs_by_status ON jobs (instance_id, status, created_at);

-- The catalog every tenant shares. A package that tenants have in common (the FHIR core, and what
-- they depend on) is stored once, under instance 0: its definitions and terminology are rows of
-- `structure_definitions` and `terminology` with `instance_id = 0`, and a tenant is given the
-- package by a row in `tenant_packages`. What belongs to each tenant is copied to it when it is
-- given the package, because it changes there: its search parameters (their status follows the
-- tenant's own indexing) and its value set expansions (a code system the tenant writes changes
-- them). These two tables hold the packages' versions of those, to copy from.
CREATE TABLE base_search_parameters (
    package_id BIGINT NOT NULL REFERENCES packages (id),
    -- The order the package lists them in: a package may define one parameter twice, and the later
    -- definition meets the earlier.
    ord BIGINT NOT NULL,
    url TEXT NOT NULL,
    base TEXT NOT NULL,
    code TEXT NOT NULL,
    param_type TEXT NOT NULL,
    expression TEXT,
    status TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, ord)
);
CREATE TABLE base_expansions (
    package_id BIGINT NOT NULL REFERENCES packages (id),
    url TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (package_id, url)
);

-- The shared catalog is read-only to a tenant's process. Only the installer that builds a shared
-- package says it may write (`SET LOCAL refhir.shared_writer = 'on'`, for one transaction), so a
-- bug in code that serves a tenant cannot change what every tenant reads. This guards against
-- mistakes, not against someone who can run any SQL in the database.
CREATE FUNCTION refhir_guard_shared_rows() RETURNS trigger LANGUAGE plpgsql AS $guard$
BEGIN
    IF current_setting('refhir.shared_writer', true) IS DISTINCT FROM 'on'
       AND ((TG_OP <> 'INSERT' AND OLD.instance_id = 0)
            OR (TG_OP <> 'DELETE' AND NEW.instance_id = 0)) THEN
        RAISE EXCEPTION 'the shared catalog (instance 0) is read-only to a tenant: % on %',
            TG_OP, TG_TABLE_NAME;
    END IF;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END
$guard$;
CREATE FUNCTION refhir_guard_shared_tables() RETURNS trigger LANGUAGE plpgsql AS $guard$
BEGIN
    IF current_setting('refhir.shared_writer', true) IS DISTINCT FROM 'on' THEN
        RAISE EXCEPTION 'the shared catalog is read-only to a tenant: % on %',
            TG_OP, TG_TABLE_NAME;
    END IF;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END
$guard$;
CREATE TRIGGER guard_shared BEFORE INSERT OR UPDATE OR DELETE ON packages
    FOR EACH ROW EXECUTE FUNCTION refhir_guard_shared_rows();
CREATE TRIGGER guard_shared BEFORE INSERT OR UPDATE OR DELETE ON structure_definitions
    FOR EACH ROW EXECUTE FUNCTION refhir_guard_shared_rows();
CREATE TRIGGER guard_shared BEFORE INSERT OR UPDATE OR DELETE ON terminology
    FOR EACH ROW EXECUTE FUNCTION refhir_guard_shared_rows();
CREATE TRIGGER guard_shared BEFORE INSERT OR UPDATE OR DELETE ON base_search_parameters
    FOR EACH ROW EXECUTE FUNCTION refhir_guard_shared_tables();
CREATE TRIGGER guard_shared BEFORE INSERT OR UPDATE OR DELETE ON base_expansions
    FOR EACH ROW EXECUTE FUNCTION refhir_guard_shared_tables();

-- Row-level security for the shared layout: the database itself shows a connection the rows of
-- the tenant it serves, and no others, whatever a statement asks for. Every statement already
-- names its tenant; this is the second line, which a statement that forgot to cannot get past.
--
-- A connection says which tenant it serves with `SET refhir.tenant = N` (`set_config`, once, as it
-- connects). A connection that has not said sees no tenant's rows at all. The catalog that tenants
-- share (instance 0) can be read by every tenant and written only by the installer that builds it,
-- which says so for one transaction with `SET LOCAL refhir.shared_writer = 'on'`.
--
-- The policies are forced, so they bind the owner of the tables too, and the owner is usually the
-- role the application connects as (it ran the migrations). A superuser, and a role with
-- BYPASSRLS, are never bound by them: a process that connects as one is not protected, and says
-- so when it starts.
--
-- FROZEN once released, like the migrations before it.

-- The tenant a connection serves, or NULL, which no row has.
CREATE FUNCTION refhir_tenant() RETURNS integer LANGUAGE sql STABLE AS $f$
    SELECT NULLIF(current_setting('refhir.tenant', true), '')::integer
$f$;
-- Whether the installer of the shared catalog has said it may write it.
CREATE FUNCTION refhir_shared_writer() RETURNS boolean LANGUAGE sql STABLE AS $f$
    SELECT COALESCE(current_setting('refhir.shared_writer', true) = 'on', false)
$f$;

-- Tables whose rows all belong to one tenant. The tenant is read once for the statement
-- (`(SELECT ...)`), not once per row.
ALTER TABLE documents ENABLE ROW LEVEL SECURITY;
ALTER TABLE documents FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON documents
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE indexes ENABLE ROW LEVEL SECURITY;
ALTER TABLE indexes FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON indexes
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE persistence_globals ENABLE ROW LEVEL SECURITY;
ALTER TABLE persistence_globals FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON persistence_globals
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE tenant_packages ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_packages FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON tenant_packages
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE search_parameters ENABLE ROW LEVEL SECURITY;
ALTER TABLE search_parameters FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON search_parameters
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE index_backfills ENABLE ROW LEVEL SECURITY;
ALTER TABLE index_backfills FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON index_backfills
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE value_set_expansion ENABLE ROW LEVEL SECURITY;
ALTER TABLE value_set_expansion FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON value_set_expansion
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE backfill_parameter ENABLE ROW LEVEL SECURITY;
ALTER TABLE backfill_parameter FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON backfill_parameter
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE smart_oauth ENABLE ROW LEVEL SECURITY;
ALTER TABLE smart_oauth FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON smart_oauth
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE jobs ENABLE ROW LEVEL SECURITY;
ALTER TABLE jobs FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON jobs
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE usage_daily ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_daily FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON usage_daily
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE request_log ENABLE ROW LEVEL SECURITY;
ALTER TABLE request_log FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON request_log
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE governance_members ENABLE ROW LEVEL SECURITY;
ALTER TABLE governance_members FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON governance_members
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

ALTER TABLE audit_log ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit_log FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_rows ON audit_log
    USING (instance_id = (SELECT refhir_tenant())) WITH CHECK (instance_id = (SELECT refhir_tenant()));

-- Tables that also hold the catalog every tenant shares (instance 0): every tenant reads it, only
-- the installer of the shared catalog writes it, and a tenant's own rows are the tenant's.
ALTER TABLE packages ENABLE ROW LEVEL SECURITY;
ALTER TABLE packages FORCE ROW LEVEL SECURITY;
CREATE POLICY shared_rows_are_read ON packages FOR SELECT
    USING (instance_id IN (0, (SELECT refhir_tenant())));
CREATE POLICY tenant_rows ON packages
    USING (instance_id = (SELECT refhir_tenant()) OR (instance_id = 0 AND refhir_shared_writer()))
    WITH CHECK (instance_id = (SELECT refhir_tenant()) OR (instance_id = 0 AND refhir_shared_writer()));

ALTER TABLE structure_definitions ENABLE ROW LEVEL SECURITY;
ALTER TABLE structure_definitions FORCE ROW LEVEL SECURITY;
CREATE POLICY shared_rows_are_read ON structure_definitions FOR SELECT
    USING (instance_id IN (0, (SELECT refhir_tenant())));
CREATE POLICY tenant_rows ON structure_definitions
    USING (instance_id = (SELECT refhir_tenant()) OR (instance_id = 0 AND refhir_shared_writer()))
    WITH CHECK (instance_id = (SELECT refhir_tenant()) OR (instance_id = 0 AND refhir_shared_writer()));

ALTER TABLE terminology ENABLE ROW LEVEL SECURITY;
ALTER TABLE terminology FORCE ROW LEVEL SECURITY;
CREATE POLICY shared_rows_are_read ON terminology FOR SELECT
    USING (instance_id IN (0, (SELECT refhir_tenant())));
CREATE POLICY tenant_rows ON terminology
    USING (instance_id = (SELECT refhir_tenant()) OR (instance_id = 0 AND refhir_shared_writer()))
    WITH CHECK (instance_id = (SELECT refhir_tenant()) OR (instance_id = 0 AND refhir_shared_writer()));

-- The versions of search parameters and expansions that tenants are given copies of: nothing in
-- them belongs to a tenant, so every tenant reads them, and only the installer writes them.
ALTER TABLE base_search_parameters ENABLE ROW LEVEL SECURITY;
ALTER TABLE base_search_parameters FORCE ROW LEVEL SECURITY;
CREATE POLICY shared_rows_are_read ON base_search_parameters FOR SELECT USING (true);
CREATE POLICY shared_writer ON base_search_parameters
    USING (refhir_shared_writer()) WITH CHECK (refhir_shared_writer());

ALTER TABLE base_expansions ENABLE ROW LEVEL SECURITY;
ALTER TABLE base_expansions FORCE ROW LEVEL SECURITY;
CREATE POLICY shared_rows_are_read ON base_expansions FOR SELECT USING (true);
CREATE POLICY shared_writer ON base_expansions
    USING (refhir_shared_writer()) WITH CHECK (refhir_shared_writer());

-- Version 4: the `near` search parameter gets an index of its own. The same change as version 3
-- of the classic layout (see `../migrations/0003_near_index.sql`), made for each tenant in turn:
-- the row-level security of version 3 shows a connection one tenant's rows and no others, so this
-- says which tenant it is working on, as a connection of that tenant does, and the setting lasts
-- for this transaction only.
--
-- FROZEN once released: add a new numbered migration instead of editing this file.

DO $migrate$
DECLARE
    tenant integer;
BEGIN
    FOR tenant IN SELECT id FROM refhir_tenants WHERE id > 0 ORDER BY id LOOP
        PERFORM set_config('refhir.tenant', tenant::text, true);

        UPDATE search_parameters SET status = 'reindexing'
        WHERE instance_id = tenant AND code = 'near' AND param_type = 'special'
          AND status = 'ready';

        INSERT INTO index_backfills
            (instance_id, package_id, watermark_ts, tablet, cursor_id, done, claimed_epoch)
        SELECT DISTINCT tenant, package_id,
            (SELECT COALESCE(MAX(ts), 0) FROM documents WHERE instance_id = tenant),
            base, NULL::text, 0, -1::bigint
        FROM search_parameters
        WHERE instance_id = tenant AND status = 'reindexing'
        ORDER BY package_id, base;

        INSERT INTO backfill_parameter (instance_id, backfill_id, url)
        SELECT tenant, b.id, p.url
        FROM index_backfills b
        JOIN search_parameters p
            ON p.instance_id = b.instance_id AND p.package_id = b.package_id
            AND p.base = b.tablet AND p.status = 'reindexing'
        WHERE b.instance_id = tenant AND b.claimed_epoch = -1;

        UPDATE search_parameters SET status = 'pending'
        WHERE instance_id = tenant AND status = 'reindexing';
        UPDATE index_backfills SET claimed_epoch = NULL
        WHERE instance_id = tenant AND claimed_epoch = -1;
    END LOOP;
    PERFORM set_config('refhir.tenant', '', true);
END
$migrate$;

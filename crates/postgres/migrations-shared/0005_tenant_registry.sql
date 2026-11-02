-- Version 5: the registry records what a tenant was created with, and a deletion that waits out a
-- grace period before the tenant's rows are removed. Only the shared layout has a registry, so the
-- classic layout has no migration like it.
--
-- `settings` is what the tenant was created with (packages, update-as-create, SMART, limits), as
-- the server that reads it understands them. A tenant with `deleted_at` set is no longer served,
-- and its rows are removed once `purge_after` has passed; until then it can be restored. Its name
-- stays taken until then. `updated_at` moves with every change, so a process can tell that the
-- registry changed by reading one row.
--
-- FROZEN once released: add a new numbered migration instead of editing this file.

ALTER TABLE refhir_tenants
    ADD COLUMN settings JSONB NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN deleted_at TIMESTAMPTZ,
    ADD COLUMN purge_after TIMESTAMPTZ;

-- Version 3: the `near` search parameter gets an index of its own. Before this release `near` read
-- every resource of the type, so a place stored then has no key in the new index. The parameter
-- goes back to `pending` and a backfill is queued for the type that holds it. The server runs the
-- backfill as it starts, and `near` is searchable again when it is done (a process that only reads
-- never loads a pending parameter, so none answers from an index that is half built). A database
-- that holds no such parameter is not changed, and one that holds no place has nothing to index:
-- its backfill finishes at once.
--
-- Only data changes here, no table does. The watermark is the newest document there is: this
-- release writes the keys of every place it stores from its first write, so only what is already
-- stored needs the backfill. Two things are borrowed for the length of this migration, to tell
-- what it changes from what was already there: the status `reindexing`, and a `claimed_epoch` of
-- -1. Neither is left behind.
--
-- FROZEN once released: add a new numbered migration instead of editing this file.

UPDATE search_parameters SET status = 'reindexing'
WHERE code = 'near' AND param_type = 'special' AND status = 'ready';

INSERT INTO index_backfills (package_id, watermark_ts, tablet, cursor_id, done, claimed_epoch)
SELECT DISTINCT package_id, (SELECT COALESCE(MAX(ts), 0) FROM documents), base, NULL::text, 0, -1::bigint
FROM search_parameters
WHERE status = 'reindexing'
ORDER BY package_id, base;

INSERT INTO backfill_parameter (backfill_id, url)
SELECT b.id, p.url
FROM index_backfills b
JOIN search_parameters p
    ON p.package_id = b.package_id AND p.base = b.tablet AND p.status = 'reindexing'
WHERE b.claimed_epoch = -1;

UPDATE search_parameters SET status = 'pending' WHERE status = 'reindexing';
UPDATE index_backfills SET claimed_epoch = NULL WHERE claimed_epoch = -1;

# Schema migrations

ReFHIR keeps its tables in SQLite or Postgres. Their shape is versioned: every database records
which schema version it has, and the binary knows which versions it can run against.

**Migrations are forward-only and not reversible. Take a backup before upgrading ReFHIR.** There
is no downgrade: a database that a newer ReFHIR has migrated cannot be used by an older one, and
the way back is to restore the backup.

## How it works

- The binary carries an ordered list of migrations, compiled in (`crates/sqlite/migrations/`,
  `crates/postgres/migrations/`). Version numbers are `1, 2, 3, ...`.
- The database has a `schema_migrations` table: `version`, `applied_at` (UTC), `checksum`
  (SHA-256 of the migration's SQL). A row means that migration is applied.
- At startup, the process compares what the database recorded with what it knows:

  | The database | Startup |
  |---|---|
  | has everything the binary knows | does nothing. It reads `schema_migrations` and takes no lock and writes nothing |
  | is behind | applies the pending migrations in order, in **one transaction**: either all of them apply or none do |
  | is new | applies every migration |
  | has no `schema_migrations` (see below) | adopts it as version 1 |
  | records a version the binary does not know | **refuses to start** (see below) |
  | records a checksum that differs from the binary's | **refuses to start**: the migration was changed after it was applied |

- Two processes starting together cannot both migrate. On Postgres the migration runs behind a
  transaction-scoped advisory lock; on SQLite it runs in a `BEGIN IMMEDIATE` transaction. The
  second process waits, then finds nothing to do.
- A process started with `--role replica` never runs DDL. It waits (up to two minutes) for the
  primary to bring the database to the version it knows, and refuses at once if the database is
  *ahead* of it, since waiting would not help.

If a migration fails, the transaction rolls back, nothing is recorded, the database is as it was,
and the process exits with the error. Fix the cause and start again.

## Databases from before versioning

Releases before this one created their tables at startup with no record of a version. A database
that has no `schema_migrations` table is **adopted** as version 1, the baseline: the current
schema.

- If it already has every table, index and column of the baseline, version 1 is **recorded and
  not run**. No DDL touches your tables. (On Postgres even `ALTER TABLE ... ADD COLUMN IF NOT
  EXISTS` takes a lock that waits for open writes, so a restart cannot stall a live database.)
- If it was made by an older release and lacks something (a `claimed_epoch` column on
  `index_backfills`, the lease columns on `jobs`, the `indexes_by_ts` index), exactly what every
  earlier release did on every start is done once: the missing objects are added, nothing is
  altered or dropped, and then version 1 is recorded.

On Postgres, adding a missing `indexes_by_ts` builds it inside the migration transaction. Releases
before this one built it `CONCURRENTLY`, outside a transaction, so writes could continue; Postgres
refuses that inside a transaction, and a migration that is not one transaction is not atomic.
This only affects a database from before that index existed, and the build blocks writes to
`indexes` while it runs. Upgrade such a database in a maintenance window.

## A database that is newer than the binary

```
the database is at schema version 3, but this build only knows versions up to 2: it was migrated
by a newer ReFHIR. Refusing to start. ...
```

An older binary cannot know what a newer migration changed, so it will not run. Use the same
ReFHIR version everywhere that shares a database, or restore a backup taken before the upgrade.
The server writes nothing before it refuses.

## Checking before you start: `refhir migrate`

```bash
refhir migrate --status [--config path] [--sqlite path | --postgres url]
refhir migrate --dry-run [...]
```

Both read the database that `refhir` would open (same config file, `REFHIR_POSTGRES_URL` and
flags) and change nothing. A SQLite file that does not exist is reported as new and is not
created. The Postgres URL is not printed.

- `--status`: the version this build knows, the versions the database recorded (name, time,
  checksum prefix), and what startup would do.
- `--dry-run`: the same, plus the SQL that would run.

It exits `1` when this build would refuse to start (a newer database, a changed checksum, a
damaged `schema_migrations`) and `0` otherwise, including when migrations are pending. It does
not apply anything; the server applies migrations itself when it starts.

## Upgrading

Restore-and-test first, then upgrade in place:

1. Back up the database (`pg_dump`, or copy the SQLite file with its `-wal`/`-shm` while ReFHIR
   is stopped).
2. Restore the backup into a scratch database and run `refhir migrate --dry-run` against it with
   the **new** binary. Start the new binary on it and test.
3. Stop every old process that uses the production database before the new one starts. An old
   binary does not look at `schema_migrations`, so it cannot notice a migration happening under
   it. Run the new version on the primary, standbys and replicas alike: a new replica waits for
   the primary's migration, but an old one does not know there is anything to wait for.
4. Start the new binary. The first process to start migrates; the rest wait and continue.

To go back, restore the backup taken in step 1.

## Adding a migration

1. Add `NNNN_short_name.sql` to **both** `crates/sqlite/migrations/` and
   `crates/postgres/migrations/`, and an entry to `MIGRATIONS` in each backend's `migrate.rs`.
   The next version number is the last one plus one, with no gaps.
2. **Never edit a migration that has shipped.** Its checksum is in every database that applied it,
   and they would refuse to start. `the_baseline_is_frozen` pins version 1's checksum; pin new
   ones the same way.
3. A migration runs inside a transaction, together with any other pending ones. Do not use
   statements Postgres refuses there (`CREATE INDEX CONCURRENTLY`, `VACUUM`). It does not have to
   be idempotent: it runs once. Take care with statements that lock a busy table, since a
   standby or replica starting up waits for them.
4. Keep it additive where you can (new tables, new nullable columns, new indexes). A migration
   that rewrites data is not undone by anything but a restore.
5. Add a test that starts from a database at the previous version.

New SQLite and Postgres migrations should leave the two backends with the same logical schema,
and neither may change what a query returns without a code change that says so.

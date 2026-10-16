# Schema migrations

ReFHIR keeps its tables in SQLite or Postgres. Their shape is versioned: every database records
which schema version it has, and the binary knows which versions it can run against.

**Migrations are forward-only and not reversible. Take a backup before upgrading ReFHIR.** There
is no downgrade: a database that a newer ReFHIR has migrated cannot be used by an older one, and
the way back is to restore the backup.

## How it works

- The binary carries an ordered list of migrations. Version numbers are `1, 2, 3, ...`.
- The database has a `schema_migrations` table: `version`, `applied_at` (UTC), `checksum`
  (SHA-256 of the migration's SQL). A row means that migration is applied.
- At startup, the process compares what the database recorded with what it knows:

  | The database | Startup |
  |---|---|
  | has everything the binary knows | does nothing. It reads `schema_migrations` and takes no lock and writes nothing |
  | is behind | applies the pending migrations in order, in **one transaction**: either all of them apply or none do |
  | is new | applies every migration |
  | has no `schema_migrations` (a database from a build before 0.0.1) | adopts it as version 1, the baseline |
  | records a version the binary does not know | **refuses to start** (see below) |
  | records a checksum that differs from the binary's | **refuses to start**: the migration was changed after it was applied |

- Two processes starting together cannot both migrate. On Postgres the migration runs behind a
  transaction-scoped advisory lock; on SQLite it runs in a `BEGIN IMMEDIATE` transaction. The
  second process waits, then finds nothing to do.
- On Postgres, a process started with `--role replica` never runs DDL. It waits (up to two
  minutes) for the primary to bring the database to the version it knows, and refuses at once if
  the database is *ahead* of it, since waiting would not help. A database that has the whole
  current schema but no `schema_migrations` (its primary still runs the previous release) is
  usable as it is, so the replica starts without waiting. On SQLite, which has no separate
  schema step for replicas, every process that opens the file migrates it, as above: start the
  primary first if you want it to be the one that does.
- On Postgres, a migration waits at most two minutes for a lock (the advisory lock another
  migrating process holds, or a table lock held by an open transaction), then fails startup with
  an error instead of hanging.

If a migration fails, the transaction rolls back, nothing is recorded, the database is as it was,
and the process exits with the error. Fix the cause and start again.

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

Exit codes: `0` when it could read the database and this build would start (including when
migrations are pending); `1` when this build would refuse to start (a newer database, a changed
checksum, a damaged `schema_migrations`); `2` when it could not tell (bad arguments, an
unreadable config file or database). A script can therefore tell "refused" from "could not
look". It does not apply anything; the server applies migrations itself when it starts.

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

## The migrations so far

| Version | Name | What it does |
|---|---|---|
| 1 | `baseline` | The schema of the last release that created its tables at startup. |
| 2 | `governance` | Adds `usage_daily`, `request_log`, `audit_log` (hash-chained) and `governance_members`, which record what each caller used, the recent requests, who touched which resource, and how many servers share the database. Purely additive. |

A replica on a database that a previous release's primary still owns waits for the primary to
apply version 2, as it would for any later migration: it never runs DDL itself.

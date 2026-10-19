import { readdir, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';

import type postgres from 'postgres';

/** The SQL of each version of the schema, for each database: `migrations/postgres/001_init.sql`,
 * `002_...`, in order. */
export const MIGRATIONS = fileURLToPath(new URL('../../migrations/', import.meta.url));
const DIRECTORY = `${MIGRATIONS}postgres/`;

/** The migrations of a directory, oldest first. */
export async function migrationFiles(directory: string): Promise<Array<{ version: number; sql: string }>> {
  const names = (await readdir(directory)).filter((name) => /^\d+_.+\.sql$/.test(name)).sort();
  return Promise.all(names.map(async (name) => ({ version: Number(name.split('_')[0]), sql: await readFile(`${directory}${name}`, 'utf8') })));
}

/**
 * Brings the schema up to date. Processes that start together take turns, so a migration is run
 * once, and one that is already done is not run again.
 */
export async function migrate(sql: postgres.Sql, directory = DIRECTORY): Promise<number[]> {
  const files = await migrationFiles(directory);
  return sql.begin(async (tx) => {
    await tx`SELECT pg_advisory_xact_lock(hashtext('refhir-link-host-migrate'))`;
    await tx`CREATE TABLE IF NOT EXISTS schema_migrations (version integer PRIMARY KEY, applied_at bigint NOT NULL)`;
    const done = new Set((await tx<{ version: number }[]>`SELECT version FROM schema_migrations`).map((row) => row.version));
    const applied: number[] = [];
    for (const { version, sql: text } of files) {
      if (done.has(version)) continue;
      await tx.unsafe(text);
      await tx`INSERT INTO schema_migrations (version, applied_at) VALUES (${version}, ${Math.floor(Date.now() / 1000)})`;
      applied.push(version);
    }
    return applied;
  }) as Promise<number[]>;
}

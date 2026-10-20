import { randomBytes } from 'node:crypto';

import postgres from 'postgres';

import type { AccessEntry, FileEntry, Link, LinkTx, Store } from '../types.js';
import { StoreBusy } from '../types.js';
import { migrate } from './migrate.js';

export interface PostgresOptions {
  url: string;
  /** The most connections to open. Default 10. */
  max?: number;
  /** Run the migrations before anything else. Default true. */
  migrate?: boolean;
  /** For tests: a schema of its own. */
  schema?: string;
  /** Wait this long for the database to be there. Default 0: fail at once. */
  waitSeconds?: number;
  /** A change that waits this long for a link held by another gives up, and the caller is told to
   * try again. Default 10. */
  lockTimeoutSeconds?: number;
}

interface LinkRow {
  id: string;
  token_hash: string;
  active: boolean;
  created_at: string;
  created_by: string;
  expires_at: string;
  ended_at: string | null;
  attempts_left: number;
  passcode_salt: string | null;
  passcode_hash: string | null;
  max_files: number;
  max_file_bytes: number;
}
interface FileRow {
  name: string;
  content_type: string;
  bytes: number;
}

const toFiles = (rows: FileRow[]): FileEntry[] => rows.map((row) => ({ id: row.name, contentType: row.content_type, bytes: row.bytes }));
const toLink = (row: LinkRow, files: FileEntry[]): Link => ({
  id: row.id,
  tokenHash: row.token_hash,
  active: row.active,
  createdAt: Number(row.created_at),
  createdBy: row.created_by,
  expiresAt: Number(row.expires_at),
  endedAt: row.ended_at === null ? null : Number(row.ended_at),
  attemptsLeft: row.attempts_left,
  passcode: row.passcode_hash && row.passcode_salt ? { salt: row.passcode_salt, hash: row.passcode_hash } : null,
  maxFiles: row.max_files,
  maxFileBytes: row.max_file_bytes,
  files,
});

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** Links in Postgres. Several processes can share one database: a link is changed in a transaction
 * that holds its row, so a guess at a passcode is counted once whichever process it reaches. */
export async function postgresStore(options: PostgresOptions): Promise<Store> {
  const sql = postgres(options.url, {
    max: options.max ?? 10,
    connect_timeout: 10,
    idle_timeout: 30,
    onnotice: () => undefined,
    connection: {
      lock_timeout: (options.lockTimeoutSeconds ?? 10) * 1000,
      statement_timeout: 30_000,
      idle_in_transaction_session_timeout: 30_000,
      application_name: 'refhir-link-host',
      ...(options.schema ? { search_path: options.schema } : {}),
    },
  });
  try {
    // The database may be starting too, as it is when both come up from one compose file.
    const deadline = Date.now() + (options.waitSeconds ?? 0) * 1000;
    for (;;) {
      try {
        await sql`SELECT 1`;
        break;
      } catch (error) {
        if (Date.now() >= deadline) throw error;
        await sleep(500);
      }
    }
    if (options.migrate ?? true) await migrate(sql);
    else {
      // A database that was not migrated would fail on its first request, which is a poor time to say so.
      const [ready] = await sql<{ found: string | null }[]>`SELECT to_regclass('links')::text AS found`;
      if (!ready?.found) throw new Error('the database has no schema: run "refhir-link-host migrate", or set AUTO_MIGRATE=true');
    }
  } catch (error) {
    await sql.end({ timeout: 1 }).catch(() => undefined);
    throw error;
  }

  const filesOf = async (db: postgres.Sql | postgres.TransactionSql, id: string) =>
    toFiles(await db<FileRow[]>`SELECT name, content_type, bytes FROM link_files WHERE link_id = ${id} ORDER BY seq`);

  const store: Store = {
    async create(link) {
      await sql`
        INSERT INTO links (id, token_hash, active, created_at, created_by, expires_at, ended_at, attempts_left,
                           passcode_salt, passcode_hash, max_files, max_file_bytes)
        VALUES (${link.id}, ${link.tokenHash}, ${link.active}, ${link.createdAt}, ${link.createdBy}, ${link.expiresAt},
                ${link.endedAt}, ${link.attemptsLeft}, ${link.passcode?.salt ?? null}, ${link.passcode?.hash ?? null},
                ${link.maxFiles}, ${link.maxFileBytes})`;
    },

    async get(id) {
      const [row] = await sql<LinkRow[]>`SELECT * FROM links WHERE id = ${id}`;
      return row ? toLink(row, await filesOf(sql, id)) : null;
    },

    async update<T>(id: string, change: (link: Link | null, tx: LinkTx) => Promise<T> | T): Promise<T> {
      const absent: LinkTx = {
        async putFile() {
          throw new Error('there is no such link');
        },
        async removeFiles() {},
        async recordAccess() {},
      };
      try {
        return (await sql.begin(async (tx) => {
          const [row] = await tx<LinkRow[]>`SELECT * FROM links WHERE id = ${id} FOR UPDATE`;
          if (!row) return change(null, absent);
          const link = toLink(row, await filesOf(tx, id));
          const held: LinkTx = {
            async putFile(file, text) {
              await tx`
                INSERT INTO link_files (link_id, seq, name, content_type, bytes, body)
                VALUES (${id}, ${link.files.length}, ${file.id}, ${file.contentType}, ${file.bytes}, ${text})`;
              link.files.push(file);
            },
            async removeFiles() {
              await tx`DELETE FROM link_files WHERE link_id = ${id}`;
              link.files = [];
            },
            async recordAccess(entry: AccessEntry, keep: number) {
              await tx`
                INSERT INTO link_access (link_id, at, recipient, outcome, ip, user_agent)
                VALUES (${id}, ${entry.at}, ${entry.recipient}, ${entry.outcome}, ${entry.ip ?? null}, ${entry.userAgent ?? null})`;
              await tx`
                DELETE FROM link_access
                WHERE link_id = ${id}
                  AND id NOT IN (SELECT id FROM link_access WHERE link_id = ${id} ORDER BY id DESC LIMIT ${keep})`;
            },
          };
          const result = await change(link, held);
          await tx`UPDATE links SET active = ${link.active}, ended_at = ${link.endedAt}, attempts_left = ${link.attemptsLeft} WHERE id = ${id}`;
          return result;
        })) as T;
      } catch (error) {
        // 55P03: the row was held by another change for longer than lock_timeout.
        if ((error as { code?: string }).code === '55P03') throw new StoreBusy(`link ${id.slice(0, 8)} is busy`);
        throw error;
      }
    },

    async getFile(id, name) {
      const [row] = await sql<{ body: string }[]>`SELECT body FROM link_files WHERE link_id = ${id} AND name = ${name}`;
      return row?.body ?? null;
    },

    async listAccess(id) {
      const rows = await sql<{ at: string; recipient: string; outcome: AccessEntry['outcome']; ip: string | null; user_agent: string | null }[]>`
        SELECT at, recipient, outcome, ip, user_agent FROM link_access WHERE link_id = ${id} ORDER BY id`;
      return rows.map((row) => ({
        recipient: row.recipient,
        at: Number(row.at),
        outcome: row.outcome,
        ...(row.ip === null ? {} : { ip: row.ip }),
        ...(row.user_agent === null ? {} : { userAgent: row.user_agent }),
      }));
    },

    async secret(name) {
      await sql`INSERT INTO host_secrets (name, value) VALUES (${name}, ${randomBytes(32).toString('base64url')}) ON CONFLICT (name) DO NOTHING`;
      const [row] = await sql<{ value: string }[]>`SELECT value FROM host_secrets WHERE name = ${name}`;
      if (!row) throw new Error(`the secret ${name} was not kept`);
      return row.value;
    },

    async sweep({ now, retainSeconds }) {
      return (await sql.begin(async (tx) => {
        // One process at a time: the others have nothing to do, and say so.
        const [lock] = await tx<{ ok: boolean }[]>`SELECT pg_try_advisory_xact_lock(hashtext('refhir-link-host-sweep')) AS ok`;
        if (!lock?.ok) return null;
        const [emptied] = await tx<{ n: number }[]>`
          WITH gone AS (
            DELETE FROM link_files
            WHERE link_id IN (SELECT id FROM links WHERE NOT active OR expires_at <= ${now})
            RETURNING link_id
          ) SELECT count(DISTINCT link_id)::int AS n FROM gone`;
        await tx`UPDATE links SET ended_at = expires_at WHERE ended_at IS NULL AND active AND expires_at <= ${now}`;
        const [forgotten] = await tx<{ n: number }[]>`
          WITH gone AS (
            DELETE FROM links WHERE ended_at IS NOT NULL AND ended_at < ${now - retainSeconds} RETURNING id
          ) SELECT count(*)::int AS n FROM gone`;
        const [live] = await tx<{ links: number; bytes: string }[]>`
          SELECT count(DISTINCT l.id)::int AS links, COALESCE(sum(f.bytes), 0)::bigint AS bytes
          FROM links l LEFT JOIN link_files f ON f.link_id = l.id
          WHERE l.active AND l.expires_at > ${now}`;
        return {
          links: live?.links ?? 0,
          bytes: Number(live?.bytes ?? 0),
          emptied: emptied?.n ?? 0,
          forgotten: forgotten?.n ?? 0,
        };
      })) as Awaited<ReturnType<Store['sweep']>>;
    },

    async ping() {
      const [row] = await sql<{ recovery: boolean; read_only: string }[]>`
        SELECT pg_is_in_recovery() AS recovery, current_setting('transaction_read_only') AS read_only`;
      if (row?.recovery || row?.read_only === 'on') throw new Error('the database is read-only');
    },

    async close() {
      await sql.end({ timeout: 5 });
    },
  };
  return store;
}

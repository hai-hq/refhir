import { randomBytes } from 'node:crypto';
import { closeSync, existsSync, mkdirSync, openSync, rmSync } from 'node:fs';
import path from 'node:path';

import type { DatabaseSync, StatementSync } from 'node:sqlite';

import type { AccessEntry, FileEntry, Link, LinkTx, Store, SweepCounts } from '../types.js';
import { StoreBusy } from '../types.js';
import { MIGRATIONS, migrationFiles } from './migrate.js';
import { inTurns } from './shared.js';

export interface SqliteOptions {
  /** The database file, made if it is not there. `:memory:` is a database that is gone with the host. */
  path: string;
  /** Make and update the schema before anything else. Default true. */
  migrate?: boolean;
  /** How long a write waits for another process's, which holds up this whole process. Default 5. */
  busyTimeoutSeconds?: number;
}

interface LinkRow {
  id: string;
  token_hash: string;
  active: number;
  created_at: number;
  created_by: string;
  expires_at: number;
  ended_at: number | null;
  attempts_left: number;
  passcode_salt: string | null;
  passcode_hash: string | null;
  max_files: number;
  max_file_bytes: number;
  version: number;
}
interface FileRow {
  name: string;
  content_type: string;
  bytes: number;
}
interface AccessRow {
  at: number;
  recipient: string;
  outcome: AccessEntry['outcome'];
  ip: string | null;
  user_agent: string | null;
}

const toFiles = (rows: FileRow[]): FileEntry[] => rows.map((row) => ({ id: row.name, contentType: row.content_type, bytes: row.bytes }));
const toLink = (row: LinkRow, files: FileEntry[]): Link => ({
  id: row.id,
  tokenHash: row.token_hash,
  active: row.active === 1,
  createdAt: row.created_at,
  createdBy: row.created_by,
  expiresAt: row.expires_at,
  endedAt: row.ended_at,
  attemptsLeft: row.attempts_left,
  passcode: row.passcode_hash && row.passcode_salt ? { salt: row.passcode_salt, hash: row.passcode_hash } : null,
  maxFiles: row.max_files,
  maxFileBytes: row.max_file_bytes,
  files,
});

/** How many times a change is run again because another process changed the link first. */
const ATTEMPTS = 50;
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

const busy = (error: unknown) => /database is locked|SQLITE_BUSY/i.test(String((error as Error)?.message ?? error));

/**
 * Links in a SQLite file, for a host that is one machine and wants no database server.
 *
 * SQLite is synchronous and has one writer, so a link is not held while a change runs, as a row is in
 * Postgres: that would hold every link at once, for as long as a passcode takes to check. A change
 * runs on a copy of the link, and is kept, in one short transaction, only if the link is still the
 * version it read. If it is not, because another process got there first, the change is run again.
 * That is what keeps a guess at a passcode counted once between processes on one file, as on one
 * machine they are. It is not for a file on a network disk, which SQLite cannot lock safely.
 */
/**
 * Node's own SQLite, loaded only by a host that keeps its links here. Node says on stderr, once, in
 * plain text, that it still calls the module experimental, which does not belong among lines of JSON:
 * that line alone is left out, and the README says it instead.
 */
let loading: Promise<typeof import('node:sqlite')> | undefined;
function loadSqlite(): Promise<typeof import('node:sqlite')> {
  // Once, in one promise: stores opened together would each save and put back `emitWarning`, and the
  // second would put back the first's wrapper for good.
  loading ??= (async () => {
    const emit = process.emitWarning;
    process.emitWarning = ((warning: string | Error, ...rest: unknown[]) => {
      if ((typeof warning === 'string' ? warning : warning.message).includes('SQLite is an experimental feature')) return;
      return (emit as (...args: unknown[]) => void).call(process, warning, ...rest);
    }) as typeof process.emitWarning;
    try {
      return await import('node:sqlite');
    } finally {
      process.emitWarning = emit;
    }
  })().catch((error) => {
    loading = undefined;
    throw error;
  });
  return loading;
}

export interface SqliteStore extends Store {
  /**
   * Writes a copy of the whole database to a new file, as it is at one moment, whatever is being
   * written meanwhile and by whichever process. It is what to back up with: the file itself is not,
   * while a host is running, because some of it is in the `-wal` file beside it.
   */
  backup(to: string): Promise<void>;
}

export async function sqliteStore(options: SqliteOptions): Promise<SqliteStore> {
  const { DatabaseSync: Database } = await loadSqlite();
  const memory = options.path === ':memory:';
  if (!memory) {
    // The database holds the hashes of passcodes and tokens, and the key that signs tickets: it is the
    // host's alone. SQLite gives the -wal and -shm files the mode of the file they belong to.
    mkdirSync(path.dirname(path.resolve(options.path)), { recursive: true, mode: 0o700 });
    closeSync(openSync(options.path, 'a', 0o600));
  }
  const db: DatabaseSync = new Database(options.path);
  try {
    // WAL lets a second process read while one writes. `busy_timeout` is how long a write waits for
    // another's, and as every call here is synchronous, how long the whole process waits.
    db.exec(`PRAGMA busy_timeout = ${Math.round((options.busyTimeoutSeconds ?? 5) * 1000)}`);
    db.exec('PRAGMA journal_mode = WAL');
    db.exec('PRAGMA synchronous = NORMAL');
    db.exec('PRAGMA foreign_keys = ON');
    if (options.migrate ?? true) await migrateSqlite(db);
    else if (!db.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'links'").get()) {
      throw new Error('the database has no schema: run "refhir-link-host migrate", or set AUTO_MIGRATE=true');
    }
  } catch (error) {
    db.close();
    throw error;
  }

  const sql = {
    link: db.prepare('SELECT * FROM links WHERE id = ?'),
    files: db.prepare('SELECT name, content_type, bytes FROM link_files WHERE link_id = ? ORDER BY seq'),
    file: db.prepare('SELECT body FROM link_files WHERE link_id = ? AND name = ?'),
    access: db.prepare('SELECT at, recipient, outcome, ip, user_agent FROM link_access WHERE link_id = ? ORDER BY id'),
    create: db.prepare(`
      INSERT INTO links (id, token_hash, active, created_at, created_by, expires_at, ended_at, attempts_left,
                         passcode_salt, passcode_hash, max_files, max_file_bytes)
      VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`),
    keep: db.prepare('UPDATE links SET active = ?, ended_at = ?, attempts_left = ?, version = version + 1 WHERE id = ? AND version = ?'),
    putFile: db.prepare('INSERT INTO link_files (link_id, seq, name, content_type, bytes, body) VALUES (?, ?, ?, ?, ?, ?)'),
    dropFiles: db.prepare('DELETE FROM link_files WHERE link_id = ?'),
    note: db.prepare('INSERT INTO link_access (link_id, at, recipient, outcome, ip, user_agent) VALUES (?, ?, ?, ?, ?, ?)'),
    prune: db.prepare('DELETE FROM link_access WHERE link_id = ? AND id NOT IN (SELECT id FROM link_access WHERE link_id = ? ORDER BY id DESC LIMIT ?)'),
    secret: db.prepare('SELECT value FROM host_secrets WHERE name = ?'),
    newSecret: db.prepare('INSERT OR IGNORE INTO host_secrets (name, value) VALUES (?, ?)'),
  } satisfies Record<string, StatementSync>;

  const row = <T>(statement: StatementSync, ...values: Array<string | number | null>) => statement.get(...values) as unknown as T | undefined;
  const rows = <T>(statement: StatementSync, ...values: Array<string | number | null>) => statement.all(...values) as unknown as T[];

  const read = (id: string): { link: Link; version: number } | null => {
    const found = row<LinkRow>(sql.link, id);
    return found ? { link: toLink(found, toFiles(rows<FileRow>(sql.files, id))), version: found.version } : null;
  };

  // One short transaction, all of it synchronous, so nothing else on this connection is in the middle.
  function inTransaction<T>(work: () => T): T {
    db.exec('BEGIN IMMEDIATE');
    try {
      const result = work();
      db.exec('COMMIT');
      return result;
    } catch (error) {
      try {
        db.exec('ROLLBACK');
      } catch {
        // already rolled back by SQLite
      }
      throw error;
    }
  }

  const inTurn = inTurns();

  const store: SqliteStore = {
    async create(link) {
      sql.create.run(
        link.id, link.tokenHash, link.active ? 1 : 0, link.createdAt, link.createdBy, link.expiresAt, link.endedAt,
        link.attemptsLeft, link.passcode?.salt ?? null, link.passcode?.hash ?? null, link.maxFiles, link.maxFileBytes,
      );
    },

    async get(id) {
      return read(id)?.link ?? null;
    },

    update<T>(id: string, change: (link: Link | null, tx: LinkTx) => Promise<T> | T): Promise<T> {
      const absent: LinkTx = {
        async putFile() {
          throw new Error('there is no such link');
        },
        async removeFiles() {},
        async recordAccess() {},
      };
      // One process takes its turns here, so that it is only another process that can overtake it.
      return inTurn(id, async () => {
        for (let attempt = 0; attempt < ATTEMPTS; attempt += 1) {
          // Overtaken: wait a moment that is not the same as the other's, so that they do not keep
          // overtaking each other.
          if (attempt > 0) await sleep(Math.random() * Math.min(2 ** attempt, 40));
          const seen = read(id);
          if (!seen) return change(null, absent);
          const { link, version } = seen;
          const puts: Array<{ file: FileEntry; seq: number; text: string }> = [];
          const notes: Array<{ entry: AccessEntry; keep: number }> = [];
          let cleared = false;
          const tx: LinkTx = {
            async putFile(file, text) {
              puts.push({ file, seq: link.files.length, text });
              link.files.push(file);
            },
            async removeFiles() {
              puts.length = 0;
              cleared = true;
              link.files = [];
            },
            async recordAccess(entry, keep) {
              notes.push({ entry, keep });
            },
          };
          const result = await change(link, tx);
          try {
            const kept = inTransaction(() => {
              const changed = sql.keep.run(link.active ? 1 : 0, link.endedAt, link.attemptsLeft, id, version);
              if (changed.changes === 0) return false; // the link is not as the change saw it
              if (cleared) sql.dropFiles.run(id);
              for (const { file, seq, text } of puts) sql.putFile.run(id, seq, file.id, file.contentType, file.bytes, text);
              for (const { entry, keep } of notes) {
                sql.note.run(id, entry.at, entry.recipient, entry.outcome, entry.ip ?? null, entry.userAgent ?? null);
                sql.prune.run(id, id, keep);
              }
              return true;
            });
            if (kept) return result;
          } catch (error) {
            if (busy(error)) throw new StoreBusy(`link ${id.slice(0, 8)} is busy`);
            throw error;
          }
        }
        throw new StoreBusy(`link ${id.slice(0, 8)} keeps changing`);
      });
    },

    async getFile(id, name) {
      return row<{ body: string }>(sql.file, id, name)?.body ?? null;
    },

    async listAccess(id) {
      return rows<AccessRow>(sql.access, id).map((found) => ({
        recipient: found.recipient,
        at: found.at,
        outcome: found.outcome,
        ...(found.ip === null ? {} : { ip: found.ip }),
        ...(found.user_agent === null ? {} : { userAgent: found.user_agent }),
      }));
    },

    async secret(name) {
      sql.newSecret.run(name, randomBytes(32).toString('base64url'));
      const found = row<{ value: string }>(sql.secret, name);
      if (!found) throw new Error(`the secret ${name} was not kept`);
      return found.value;
    },

    async sweep({ now, retainSeconds }) {
      const ended = 'SELECT id FROM links WHERE active = 0 OR expires_at <= ?';
      return inTransaction((): SweepCounts => {
        const emptied = row<{ n: number }>(db.prepare(`SELECT count(DISTINCT link_id) AS n FROM link_files WHERE link_id IN (${ended})`), now)?.n ?? 0;
        // A change that read one of these links, in another process, is not kept: it is run again.
        db.prepare(`UPDATE links SET version = version + 1 WHERE id IN (SELECT link_id FROM link_files WHERE link_id IN (${ended}))`).run(now);
        db.prepare(`DELETE FROM link_files WHERE link_id IN (${ended})`).run(now);
        db.prepare('UPDATE links SET ended_at = expires_at, version = version + 1 WHERE ended_at IS NULL AND active = 1 AND expires_at <= ?').run(now);
        const cutoff = now - retainSeconds;
        const forgotten = row<{ n: number }>(db.prepare('SELECT count(*) AS n FROM links WHERE ended_at IS NOT NULL AND ended_at < ?'), cutoff)?.n ?? 0;
        db.prepare('DELETE FROM links WHERE ended_at IS NOT NULL AND ended_at < ?').run(cutoff);
        const live = row<{ links: number; bytes: number }>(
          db.prepare(`
            SELECT count(DISTINCT l.id) AS links, COALESCE(SUM(f.bytes), 0) AS bytes
            FROM links l LEFT JOIN link_files f ON f.link_id = l.id
            WHERE l.active = 1 AND l.expires_at > ?`),
          now,
        );
        return { links: live?.links ?? 0, bytes: live?.bytes ?? 0, emptied, forgotten };
      });
    },

    async ping() {
      // Taking the write lock, and letting it go, is what says that a write would work.
      inTransaction(() => undefined);
    },

    async backup(to) {
      if (memory) throw new Error('a database in memory has nothing to back up to');
      const target = path.resolve(to);
      if (existsSync(target)) throw new Error(`${target} exists: a backup is made to a new file`);
      mkdirSync(path.dirname(target), { recursive: true, mode: 0o700 });
      // The copy is as private as the database: it is made, empty, with that mode, and VACUUM INTO,
      // which accepts an empty file, fills it. It reads one consistent snapshot, and does not stop
      // anyone writing meanwhile.
      closeSync(openSync(target, 'wx', 0o600));
      try {
        db.prepare('VACUUM INTO ?').run(target);
      } catch (error) {
        rmSync(target, { force: true });
        throw error;
      }
    },

    async close() {
      db.close();
    },
  };
  return store;
}

/** Brings the schema up to date. Processes that start together take turns, so a migration is run once. */
export async function migrateSqlite(db: DatabaseSync, directory = `${MIGRATIONS}sqlite/`): Promise<number[]> {
  const files = await migrationFiles(directory);
  // A host that starts while another is writing has nothing to wait for if the schema is up to date.
  const table = db.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'").get();
  if (table) {
    const have = new Set((db.prepare('SELECT version FROM schema_migrations').all() as unknown as Array<{ version: number }>).map((found) => found.version));
    if (files.every(({ version }) => have.has(version))) return [];
  }
  db.exec('BEGIN IMMEDIATE');
  try {
    db.exec('CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL) STRICT');
    const done = new Set((db.prepare('SELECT version FROM schema_migrations').all() as unknown as Array<{ version: number }>).map((found) => found.version));
    const applied: number[] = [];
    for (const { version, sql: text } of files) {
      if (done.has(version)) continue;
      db.exec(text);
      db.prepare('INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)').run(version, Math.floor(Date.now() / 1000));
      applied.push(version);
    }
    db.exec('COMMIT');
    return applied;
  } catch (error) {
    db.exec('ROLLBACK');
    throw error;
  }
}

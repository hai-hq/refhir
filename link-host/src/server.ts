#!/usr/bin/env node
// Start a link host from the environment. `refhir-link-host --help` says how; the README lists the
// settings.
import { createHash, randomBytes } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';

import { createHost } from './app.js';
import { ConfigError, loadConfig } from './config.js';
import { listen } from './node.js';
import { memoryStore } from './store/memory.js';
import { postgresStore } from './store/postgres.js';
import { sqliteStore, type SqliteStore } from './store/sqlite.js';
import type { LogEntry, Store } from './types.js';

const USAGE = `Usage: refhir-link-host [--check | --help | --version]
       refhir-link-host migrate
       refhir-link-host backup <file>
       refhir-link-host new-token

Starts a SMART Health Links host. It is set by environment variables, and needs DATABASE_URL (a Postgres
address, or sqlite: and the path of a file), a token that may make links (ADMIN_TOKEN, or CLIENTS_FILE)
and, unless it listens on localhost only, PUBLIC_URL. See the README for the rest.

  --check      read the settings, make sure the database can be written, and stop
  migrate      bring the database's schema up to date, and stop
  backup       copy a SQLite database to a new file, as it is at one moment, while the host runs
  new-token    print a token for a client, and the tokenHash to put in CLIENTS_FILE in its place
`;

const args = process.argv.slice(2);
if (args.includes('--help') || args.includes('-h')) {
  process.stdout.write(USAGE);
  process.exit(0);
}
if (args.includes('--version')) {
  const manifest = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8')) as { version: string };
  console.log(manifest.version);
  process.exit(0);
}
if (args[0] === 'new-token') {
  const token = randomBytes(32).toString('base64url');
  console.log(`token:     ${token}`);
  console.log(`tokenHash: sha256:${createHash('sha256').update(token).digest('hex')}`);
  console.error('Give the client the token, and put the tokenHash in CLIENTS_FILE: the host checks a token against its hash and never keeps the token.');
  process.exit(0);
}

let config: ReturnType<typeof loadConfig>;
try {
  config = loadConfig(process.env);
} catch (error) {
  if (!(error instanceof ConfigError)) throw error;
  console.error('The link host cannot start:');
  for (const problem of error.problems) console.error(`  - ${problem}`);
  process.exit(1);
}
const { options, runtime, notes } = config;

const say = (entry: Omit<LogEntry, 'ts'>) => (entry.level === 'error' ? console.error : console.log)(JSON.stringify({ ts: new Date().toISOString(), ...entry }));
const logger = (entry: LogEntry) => {
  if (entry.msg === 'request' && !runtime.logRequests) return;
  (entry.level === 'error' ? console.error : console.log)(JSON.stringify(entry));
};

let store: Store;
try {
  const kept = runtime.database;
  // A backup is of a database that is there: opening a mistyped path would make an empty one.
  if (args[0] === 'backup' && kept.kind === 'sqlite' && kept.path !== ':memory:' && !existsSync(kept.path)) {
    console.error(`There is no database at ${kept.path} to back up: check DATABASE_URL.`);
    process.exit(1);
  }
  store =
    kept.kind === 'memory'
      ? memoryStore()
      : kept.kind === 'sqlite'
        ? await sqliteStore({ path: kept.path, migrate: (kept.migrate && args[0] !== 'backup') || args[0] === 'migrate' })
        : await postgresStore({
            url: kept.url,
            max: kept.poolMax,
            waitSeconds: kept.waitSeconds,
            migrate: (kept.migrate && args[0] !== 'backup') || args[0] === 'migrate',
          });
} catch (error) {
  console.error(`The database cannot be used: ${(error as Error).message}`);
  process.exit(1);
}

if (args[0] === 'migrate') {
  console.log('The database is up to date.');
  await store.close();
  process.exit(0);
}

if (args[0] === 'backup') {
  const to = args[1];
  if (!to) {
    console.error('Say where to put the copy: refhir-link-host backup /data/backup.db');
    process.exit(1);
  }
  if (runtime.database.kind !== 'sqlite') {
    console.error('backup is for a SQLite database. A Postgres is backed up with pg_dump, or whatever backs up the rest of your databases.');
    process.exit(1);
  }
  try {
    await (store as SqliteStore).backup(to);
  } catch (error) {
    console.error(`The backup was not made: ${(error as Error).message}`);
    process.exit(1);
  }
  console.log(`A copy of the database is in ${to}.`);
  await store.close();
  process.exit(0);
}

const host = await createHost({ ...options, store, logger });

if (args.includes('--check')) {
  try {
    await store.ping();
  } catch (error) {
    console.error(`The database cannot be written: ${(error as Error).message}`);
    process.exit(1);
  }
  for (const note of notes) console.error(`note: ${note}`);
  console.log('The settings are fine and the database can be written.');
  host.close();
  await store.close();
  process.exit(0);
}

for (const note of notes) say({ level: 'warn', msg: note });
const server = await listen(host, {
  port: runtime.port,
  hostname: runtime.host,
  headersTimeoutSeconds: runtime.headersTimeoutSeconds,
  requestTimeoutSeconds: runtime.requestTimeoutSeconds,
});
say({
  level: 'info',
  msg: 'listening',
  address: `${runtime.host}:${runtime.port}`,
  publicUrl: options.publicUrl,
  store: runtime.database.kind,
  clients: options.clients.map((client) => client.name),
});

// A container is stopped with SIGTERM. The host says it is not ready, so that a balancer stops
// sending it requests, after `SHUTDOWN_DELAY_SECONDS` stops listening, finishes what is being
// answered for up to `SHUTDOWN_GRACE_SECONDS`, closes the database, and goes.
let stopping = false;
function stop(signal: string) {
  if (stopping) return;
  stopping = true;
  say({ level: 'info', msg: 'stopping', signal });
  host.drain();
  setTimeout(() => {
    server.close(() => {
      host.close();
      void store.close().finally(() => process.exit(0));
    });
    server.closeIdleConnections();
    setTimeout(() => {
      server.closeAllConnections();
      process.exit(0);
    }, runtime.shutdownGraceSeconds * 1000).unref();
  }, runtime.shutdownDelaySeconds * 1000);
}
for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => stop(signal));

// A failure nobody planned for leaves the process in a state nobody checked: let the supervisor start
// a fresh one.
for (const event of ['uncaughtException', 'unhandledRejection']) {
  process.on(event, (error) => {
    say({ level: 'error', msg: event, error: String((error as Error)?.stack ?? error) });
    process.exit(1);
  });
}

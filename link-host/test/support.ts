import { spawn, type ChildProcess } from 'node:child_process';
import { createDecipheriv, randomBytes, webcrypto } from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import postgres from 'postgres';

import { createHost, type Host } from '../src/app.js';
import { listen } from '../src/node.js';
import { memoryStore } from '../src/store/memory.js';
import { postgresStore, type PostgresOptions } from '../src/store/postgres.js';
import { sqliteStore, type SqliteOptions } from '../src/store/sqlite.js';
import type { HostOptions, Link, Store } from '../src/types.js';

/** A Postgres to test against: `LINK_HOST_TEST_DATABASE_URL=postgres://user:pass@host:5432/db`. */
export const databaseUrl = process.env.LINK_HOST_TEST_DATABASE_URL;

/** A store with a schema of its own, which goes when it is done with. */
export async function schemaStore(options: Partial<PostgresOptions> = {}) {
  if (!databaseUrl) throw new Error('LINK_HOST_TEST_DATABASE_URL is not set');
  const schema = `shl_${randomBytes(6).toString('hex')}`;
  const admin = postgres(databaseUrl, { max: 1, onnotice: () => undefined });
  await admin`CREATE SCHEMA ${admin(schema)}`;
  const stores: Store[] = [];
  return {
    schema,
    admin,
    /** Another store on the same schema, as another process would be. */
    open: async (more: Partial<PostgresOptions> = {}) => {
      const store = await postgresStore({ url: databaseUrl, schema, ...options, ...more });
      stores.push(store);
      return store;
    },
    /** A connection to the same schema, for what the test does behind the store's back. */
    raw: () => postgres(databaseUrl, { max: 1, onnotice: () => undefined, connection: { search_path: schema } }),
    done: async () => {
      await Promise.allSettled(stores.map((store) => store.close()));
      await admin`DROP SCHEMA ${admin(schema)} CASCADE`;
      await admin.end({ timeout: 2 });
    },
  };
}

/** A SQLite file in a directory of its own, which goes when the test is done with it. */
export function sqliteFile() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'link-sqlite-'));
  const file = path.join(dir, 'links.db');
  const stores: Store[] = [];
  return {
    dir,
    file,
    /** Another store on the same file, as another process would be. */
    open: async (options: Partial<SqliteOptions> = {}) => {
      const store = await sqliteStore({ path: file, ...options });
      stores.push(store);
      return store;
    },
    done: async () => {
      await Promise.allSettled(stores.map((store) => store.close()));
      fs.rmSync(dir, { recursive: true, force: true });
    },
  };
}

export const memory = (now?: () => number) => ({ store: memoryStore(now ? { now } : {}), done: async () => undefined });

/** What the suite keeps links in: memory, or with `LINK_HOST_TEST_STORE=sqlite` a file of its own, or
 * with `LINK_HOST_TEST_STORE=postgres` a schema of its own in `LINK_HOST_TEST_DATABASE_URL`, so that
 * every test of the host runs against each. */
export async function storeForTests(now: () => number): Promise<{ store: Store; done: () => Promise<unknown> }> {
  if (process.env.LINK_HOST_TEST_STORE === 'sqlite') {
    const sqlite = sqliteFile();
    return { store: await sqlite.open(), done: sqlite.done };
  }
  if (process.env.LINK_HOST_TEST_STORE === 'postgres') {
    const schema = await schemaStore();
    return { store: await schema.open(), done: schema.done };
  }
  return memory(now);
}

export const ID = (letter: string) => letter.repeat(43);

export function aLink(id: string, extra: Partial<Link> = {}): Link {
  const now = Math.floor(Date.now() / 1000);
  return {
    id,
    tokenHash: 'ab'.repeat(32),
    active: true,
    createdAt: now,
    createdBy: 'test',
    expiresAt: now + 86_400,
    endedAt: null,
    attemptsLeft: 5,
    passcode: null,
    maxFiles: 10,
    maxFileBytes: 1_000_000,
    files: [],
    ...extra,
  };
}

// ---- a host to call, and the calls a sender and a receiver make ----

export const ADMIN = 'an-admin-secret-of-length';
export const SUMMARY = JSON.stringify({ resourceType: 'Bundle', type: 'document', entry: [{ note: 'private-health-detail' }] });
const NO_LIMITS = { public: 0, perLink: 0, management: 0, create: 0, authFailures: 0 };

const b64 = (bytes: Uint8Array) => Buffer.from(bytes).toString('base64url');

/** A file the way a sender makes it: a compact JWE, direct and A256GCM. The host never opens one;
 * the tests make them so the host has something of the right shape to keep. */
export async function encryptFile(text: string, key: Uint8Array, contentType = 'application/fhir+json'): Promise<string> {
  const header = b64(Buffer.from(JSON.stringify({ alg: 'dir', enc: 'A256GCM', cty: contentType })));
  const iv = webcrypto.getRandomValues(new Uint8Array(12));
  const secret = await webcrypto.subtle.importKey('raw', key, 'AES-GCM', false, ['encrypt']);
  const sealed = new Uint8Array(
    await webcrypto.subtle.encrypt({ name: 'AES-GCM', iv, additionalData: new TextEncoder().encode(header), tagLength: 128 }, secret, new TextEncoder().encode(text)),
  );
  return `${header}..${b64(iv)}.${b64(sealed.slice(0, -16))}.${b64(sealed.slice(-16))}`;
}

export interface Running {
  host: Host;
  base: string;
  port: number;
  /** Moves the clock the host reads, in seconds. */
  advance(seconds: number): void;
  now(): number;
  stop(): Promise<void>;
}

/** A host on a port of its own, with a clock the test can move, and no limits unless a test asks. */
export async function startHost(
  options: Partial<HostOptions> & { headersTimeoutSeconds?: number; requestTimeoutSeconds?: number } = {},
): Promise<Running> {
  let clock = Date.now();
  const { rateLimits, headersTimeoutSeconds, requestTimeoutSeconds, store: given, ...rest } = options;
  const { store, done } = given ? { store: given, done: async () => undefined } : await storeForTests(() => clock);
  let host: Host;
  try {
    host = await createHost({
      adminToken: ADMIN,
      store,
      now: () => clock,
      sweepIntervalSeconds: 0,
      rateLimits: { ...NO_LIMITS, ...rateLimits },
      ...rest,
    });
  } catch (error) {
    await done();
    throw error;
  }
  const server = await listen(host, {
    port: 0,
    hostname: '127.0.0.1',
    ...(headersTimeoutSeconds ? { headersTimeoutSeconds } : {}),
    ...(requestTimeoutSeconds ? { requestTimeoutSeconds } : {}),
  });
  const port = (server.address() as net.AddressInfo).port;
  return {
    host,
    base: `http://127.0.0.1:${port}`,
    port,
    advance: (seconds) => void (clock += seconds * 1000),
    now: () => clock,
    stop: async () => {
      host.close();
      server.closeAllConnections();
      await new Promise((resolve) => server.close(resolve));
      await done();
    },
  };
}

export interface Reply {
  status: number;
  text: string;
  json: any;
  headers: Headers;
}

export async function call(
  url: string,
  { method = 'GET', token, body, headers = {} }: { method?: string; token?: string; body?: unknown; headers?: Record<string, string> } = {},
): Promise<Reply> {
  const response = await fetch(url, {
    method,
    headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), ...headers },
    body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body),
  });
  const text = await response.text();
  let json: unknown;
  try {
    json = JSON.parse(text);
  } catch {
    json = undefined;
  }
  return { status: response.status, text, json, headers: response.headers };
}

export const makeLink = (host: { base: string }, options: Record<string, unknown> = {}, token = ADMIN) =>
  call(`${host.base}/shl`, { method: 'POST', token, body: options });
export const manifest = (url: string, body: unknown, headers?: Record<string, string>) => call(url, { method: 'POST', body, headers });

export const randomKey = () => crypto.getRandomValues(new Uint8Array(32));

export interface Made {
  id: string;
  manageToken: string;
  manifestUrl: string;
  expiresAt: number;
  key: Uint8Array;
}

export async function putFile(host: { base: string }, link: { id: string; manageToken: string }, text = SUMMARY, key = randomKey(), query = '') {
  return call(`${host.base}/shl/${link.id}/file${query}`, {
    method: 'POST',
    token: link.manageToken,
    body: await encryptFile(text, key),
    headers: { 'content-type': 'application/jose' },
  });
}

export async function linkWithFile(host: { base: string }, options: Record<string, unknown> = {}, text = SUMMARY): Promise<Made> {
  const key = randomKey();
  const made = (await makeLink(host, options)).json;
  const put = await putFile(host, made, text, key, `?contentType=${encodeURIComponent('application/fhir+json;fhirVersion=4.0.1')}`);
  if (put.status !== 201) throw new Error(`a file was not put on the link: ${put.text}`);
  return { ...made, key };
}

export function open(jwe: string, key: Uint8Array): string {
  const [header, , iv, ciphertext, tag] = jwe.split('.') as [string, string, string, string, string];
  const decipher = createDecipheriv('aes-256-gcm', key, Buffer.from(iv, 'base64url'));
  decipher.setAAD(Buffer.from(header));
  decipher.setAuthTag(Buffer.from(tag, 'base64url'));
  return Buffer.concat([decipher.update(Buffer.from(ciphertext, 'base64url')), decipher.final()]).toString('utf8');
}

export async function freePort(): Promise<number> {
  return new Promise((resolve) => {
    const probe = net.createServer().listen(0, '127.0.0.1', () => {
      const { port } = probe.address() as net.AddressInfo;
      probe.close(() => resolve(port));
    });
  });
}

/** A request written by hand, so that a test can send half of it and wait. */
export function rawRequest(port: number, head: string, body = ''): net.Socket {
  const socket = net.connect(port, '127.0.0.1');
  socket.write(head + body);
  return socket;
}

/** What comes back on a socket within `ms`, as text; empty if nothing does. */
export function readSome(socket: net.Socket, ms = 2000): Promise<string> {
  return new Promise((resolve) => {
    let text = '';
    const finish = () => {
      clearTimeout(timer);
      socket.off('data', onData);
      resolve(text);
    };
    const onData = (chunk: Buffer) => {
      text += chunk.toString('utf8');
      if (text.includes('\r\n\r\n')) finish();
    };
    const timer = setTimeout(finish, ms);
    socket.on('data', onData);
    socket.once('close', finish);
  });
}

// ---- the command, started the way an operator starts it ----

const serverPath = fileURLToPath(new URL('../src/server.ts', import.meta.url));

export interface Cli {
  base: string;
  port: number;
  child: ChildProcess;
  exited: Promise<{ code: number | null; signal: NodeJS.Signals | null }>;
  stop(): Promise<void>;
  output(): string;
  errors(): string;
  lines(): Array<Record<string, any>>;
  errorLines(): Array<Record<string, any>>;
}

/** Runs the command with the given environment, to the end: for what stops it, or prints and goes. */
export function runCli(args: string[], env: Record<string, string>): Promise<{ code: number | null; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    const child = spawn('node', ['--import', 'tsx', serverPath, ...args], { env: { PATH: process.env.PATH ?? '', ...env } });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => (stdout += chunk));
    child.stderr.on('data', (chunk) => (stderr += chunk));
    child.on('close', (code) => resolve({ code, stdout, stderr }));
  });
}

const parseLines = (text: string) =>
  text
    .split('\n')
    .filter(Boolean)
    .flatMap((line) => {
      try {
        return [JSON.parse(line)];
      } catch {
        return [];
      }
    });

/** The command, running, on a port of its own. It keeps links in memory unless it is given a database. */
export async function startCli(env: Record<string, string> = {}, { port }: { port?: number } = {}): Promise<Cli> {
  const listenOn = port ?? (await freePort());
  const child = spawn('node', ['--import', 'tsx', serverPath], {
    env: {
      PATH: process.env.PATH ?? '',
      ADMIN_TOKEN: ADMIN,
      HOST: '127.0.0.1',
      PORT: String(listenOn),
      PUBLIC_URL: `http://127.0.0.1:${listenOn}`,
      STORE: 'memory',
      ...env,
    },
  });
  let output = '';
  let errors = '';
  child.stdout.on('data', (chunk) => (output += chunk));
  child.stderr.on('data', (chunk) => (errors += chunk));
  const exited = new Promise<{ code: number | null; signal: NodeJS.Signals | null }>((resolve) =>
    child.once('exit', (code, signal) => resolve({ code, signal })),
  );
  const base = `http://127.0.0.1:${listenOn}`;
  for (let attempt = 0; attempt < 150; attempt += 1) {
    const answered = await fetch(`${base}/healthz`).then(
      (reply) => reply.ok,
      () => false,
    );
    if (answered) break;
    if (child.exitCode !== null) throw new Error(`the command stopped: ${errors}${output}`);
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  return {
    base,
    port: listenOn,
    child,
    exited,
    stop: async () => {
      child.kill('SIGKILL');
      await exited;
    },
    output: () => output,
    errors: () => errors,
    lines: () => parseLines(output),
    errorLines: () => parseLines(errors),
  };
}

export const tempFile = (name: string, content: string) => {
  const dir = fs.mkdtempSync(`${process.env.TMPDIR ?? '/tmp'}/link-host-`);
  const file = `${dir}/${name}`;
  fs.writeFileSync(file, content);
  return { file, dir, clean: () => fs.rmSync(dir, { recursive: true, force: true }) };
};

/** A host for the length of one test. */
export async function using(options: Parameters<typeof startHost>[0], test: (host: Running) => Promise<void>): Promise<void>;
export async function using(test: (host: Running) => Promise<void>): Promise<void>;
export async function using(first: unknown, second?: unknown): Promise<void> {
  const [options, test] = (second ? [first, second] : [{}, first]) as [Parameters<typeof startHost>[0], (host: Running) => Promise<void>];
  const host = await startHost(options);
  try {
    await test(host);
  } finally {
    await host.stop();
  }
}

/** A database of its own on the server of `LINK_HOST_TEST_DATABASE_URL`, for the command, which can
 * only be given a whole database. */
export async function scratchDatabase() {
  if (!databaseUrl) throw new Error('LINK_HOST_TEST_DATABASE_URL is not set');
  const name = `shl_${randomBytes(6).toString('hex')}`;
  const admin = postgres(databaseUrl, { max: 1, onnotice: () => undefined });
  const url = new URL(databaseUrl);
  url.pathname = `/${name}`;
  return {
    name,
    url: url.toString(),
    create: async () => void (await admin`CREATE DATABASE ${admin(name)}`),
    connect: () => postgres(url.toString(), { max: 1, onnotice: () => undefined }),
    done: async () => {
      await admin`DROP DATABASE IF EXISTS ${admin(name)} WITH (FORCE)`;
      await admin.end({ timeout: 2 });
    },
  };
}

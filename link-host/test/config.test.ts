import { describe, expect, it } from 'vitest';

import { ConfigError, loadConfig } from '../src/config.js';

const TOKEN = 'a-sixteen-char-token';
const DB = 'postgres://user:pw@db.example:5432/links';
const files = (entries: Record<string, string>) => ({
  read: (file: string) => {
    if (!(file in entries)) throw new Error('no such file');
    return entries[file]!;
  },
});
const problemsOf = (env: Record<string, string>, reader?: ReturnType<typeof files>) => {
  try {
    loadConfig(env, reader);
  } catch (error) {
    expect(error).toBeInstanceOf(ConfigError);
    return (error as ConfigError).problems;
  }
  return [];
};
const BASE = { ADMIN_TOKEN: TOKEN, DATABASE_URL: DB, PUBLIC_URL: 'https://links.example.org' };

describe('the settings of a host', () => {
  it('need, on localhost, only a token and a database, and say what the host is doing', () => {
    const { options, runtime, notes } = loadConfig({ ADMIN_TOKEN: TOKEN, DATABASE_URL: DB, HOST: '127.0.0.1', PORT: '9000' });
    expect(options.publicUrl).toBe('http://localhost:9000');
    expect(options.clients).toEqual([{ name: 'admin', token: TOKEN }]);
    expect(runtime.port).toBe(9000);
    expect(runtime.host).toBe('127.0.0.1');
    expect(runtime.database).toEqual({ kind: 'postgres', url: DB, poolMax: 10, waitSeconds: 30, migrate: true });
    expect(options.trustProxy).toBe(0);
    expect(options.corsOrigins).toEqual(['*']);
    expect(options.managementCorsOrigins).toEqual([]);
    expect(notes).toEqual([]);
  });

  it('are what a host is meant to have, when they are not set', () => {
    const { options, runtime } = loadConfig({ ...BASE, PUBLIC_URL: 'https://links.example.org/' });
    expect(options.publicUrl).toBe('https://links.example.org'); // no trailing slash
    expect(runtime.host).toBe('0.0.0.0');
    expect(runtime.port).toBe(8090);
    expect(options.defaultLifetimeSeconds).toBe(30 * 86_400);
    expect(options.maxLifetimeSeconds).toBe(365 * 86_400);
    expect(options.retainSeconds).toBe(30 * 86_400);
    expect(options.sweepIntervalSeconds).toBe(600);
    expect(options.maxFileBytes).toBe(10 * 1024 * 1024);
    expect(options.maxFilesPerLink).toBe(10);
    expect(options.rateLimits?.public).toBe(120);
    expect(options.recordClientInfo).toBe(true);
    expect(runtime.logRequests).toBe(true);
    expect(runtime.headersTimeoutSeconds).toBe(10);
    expect(runtime.requestTimeoutSeconds).toBe(120);
  });

  it('are all reported at once, and each says what is wanted', () => {
    const problems = problemsOf({
      ADMIN_TOKEN: 'short',
      PUBLIC_URL: 'http://links.example.org',
      DATABASE_URL: 'mysql://nope',
      PORT: 'eighty',
      MAX_FILES_PER_LINK: '0',
      TRUST_PROXY: '-1',
      REQUIRE_PASSCODE: 'maybe',
      DEFAULT_ATTEMPTS: '9',
      MAX_ATTEMPTS: '3',
      ALLOWED_CONTENT_TYPES: 'json',
    });
    const text = problems.join('\n');
    for (const wanted of [
      /ADMIN_TOKEN is a secret of 16 characters or more/,
      /PUBLIC_URL has to be https/,
      /DATABASE_URL is postgres:\/\/user:password@host:5432\/database, or sqlite: and the path of a file/,
      /PORT is a whole number from 1 to 65535, not "eighty"/,
      /MAX_FILES_PER_LINK is a whole number from 1 to 1000/,
      /TRUST_PROXY is a whole number from 0 to 10/,
      /REQUIRE_PASSCODE is true or false, not "maybe"/,
      /DEFAULT_ATTEMPTS is more than MAX_ATTEMPTS/,
      /ALLOWED_CONTENT_TYPES is a comma-separated list of media types/,
    ]) {
      expect(text).toMatch(wanted);
    }
    expect(problems.length).toBeGreaterThanOrEqual(9);
  });

  it('refuse an address a link could not be made for', () => {
    const base = { ADMIN_TOKEN: TOKEN, DATABASE_URL: DB };
    expect(problemsOf(base).join()).toMatch(/Set PUBLIC_URL/); // on every interface it has to be told its address
    expect(problemsOf({ ...base, PUBLIC_URL: 'not an address' }).join()).toMatch(/not an address/);
    expect(problemsOf({ ...base, PUBLIC_URL: 'https://x.example/?a=1' }).join()).toMatch(/no query or fragment/);
    expect(problemsOf({ ...base, PUBLIC_URL: 'ftp://x.example' }).join()).toMatch(/http or https/);
    expect(problemsOf({ ...base, PUBLIC_URL: 'http://localhost:8090' })).toEqual([]); // a local address may be http
    expect(loadConfig({ ...base, PUBLIC_URL: 'https://x.example/links/' }).options.publicUrl).toBe('https://x.example/links'); // a path is kept
  });

  it('say where links are kept: Postgres, a SQLite file, or memory when asked, and nothing else', () => {
    const base = { ADMIN_TOKEN: TOKEN, PUBLIC_URL: 'https://x.example' };
    expect(problemsOf(base).join()).toMatch(/Set DATABASE_URL/);
    const memory = loadConfig({ ...base, STORE: 'memory' });
    expect(memory.runtime.database).toEqual({ kind: 'memory' });
    expect(memory.notes.join()).toMatch(/lost when the host stops/);
    expect(problemsOf({ ...base, STORE: 'mysql' }).join()).toMatch(/STORE is memory, postgres or sqlite/);
    const tuned = loadConfig({ ...base, DATABASE_URL: DB, DATABASE_POOL_MAX: '3', DATABASE_WAIT_SECONDS: '0', AUTO_MIGRATE: 'false' });
    expect(tuned.runtime.database).toEqual({ kind: 'postgres', url: DB, poolMax: 3, waitSeconds: 0, migrate: false });
    expect(problemsOf({ ...base, DATABASE_URL: DB, DATABASE_POOL_MAX: '0' }).join()).toMatch(/DATABASE_POOL_MAX is a whole number from 1 to 200/);

    // The kind of address says which database: sqlite: and a path, absolute or not.
    for (const [url, file] of [
      ['sqlite:///data/links.db', '/data/links.db'],
      ['sqlite:/data/links.db', '/data/links.db'],
      ['sqlite:links.db', 'links.db'],
      ['sqlite:./state/links.db', './state/links.db'],
      ['SQLITE:///data/links.db', '/data/links.db'],
    ] as const) {
      expect(loadConfig({ ...base, DATABASE_URL: url }).runtime.database, url).toEqual({ kind: 'sqlite', path: file, migrate: true });
    }
    expect(loadConfig({ ...base, DATABASE_URL: 'sqlite:///data/links.db', AUTO_MIGRATE: 'false' }).runtime.database).toEqual({ kind: 'sqlite', path: '/data/links.db', migrate: false });
    expect(loadConfig({ ...base, DATABASE_URL: 'sqlite::memory:' }).notes.join()).toMatch(/lost when the host stops/);
    expect(problemsOf({ ...base, DATABASE_URL: 'sqlite:' }).join()).toMatch(/sqlite: and the path of a file/);
    // A setting that says the other database than the address is a mistake, and is said.
    expect(problemsOf({ ...base, DATABASE_URL: DB, STORE: 'sqlite' }).join()).toMatch(/STORE is sqlite, and DATABASE_URL is a Postgres address/);
    expect(problemsOf({ ...base, DATABASE_URL: 'sqlite:///data/links.db', STORE: 'postgres' }).join()).toMatch(/STORE is postgres, and DATABASE_URL is a SQLite file/);
    expect(loadConfig({ ...base, DATABASE_URL: 'sqlite:///data/links.db', STORE: 'sqlite' }).runtime.database.kind).toBe('sqlite');
  });

  it('say who may make links: the admin token, a file of clients, or both', () => {
    const clients = JSON.stringify({
      clients: [
        { name: 'ehr', tokenHash: `sha256:${'ab'.repeat(32)}`, maxLifetimeDays: 7, defaultLifetimeDays: 2, maxFilesPerLink: 2, requirePasscode: true },
        { name: 'portal', token: 'portal-token-of-sixteen' },
      ],
    });
    const reader = files({ '/clients.json': clients });
    const { options } = loadConfig({ CLIENTS_FILE: '/clients.json', DATABASE_URL: DB, PUBLIC_URL: 'https://x.example' }, reader);
    expect(options.clients.map((client) => client.name)).toEqual(['ehr', 'portal']);
    expect(options.clients[0]).toMatchObject({ maxLifetimeSeconds: 7 * 86_400, defaultLifetimeSeconds: 2 * 86_400, requirePasscode: true });
    expect(options.clients[0]).not.toHaveProperty('maxLifetimeDays'); // days became seconds
    const both = loadConfig({ CLIENTS_FILE: '/clients.json', ADMIN_TOKEN: TOKEN, DATABASE_URL: DB, PUBLIC_URL: 'https://x.example' }, reader);
    expect(both.options.clients.map((client) => client.name)).toEqual(['admin', 'ehr', 'portal']);
  });

  it('refuse a clients file with a mistake in it, and say which client and what', () => {
    const env = { CLIENTS_FILE: '/c.json', DATABASE_URL: DB, PUBLIC_URL: 'https://x.example' };
    const refuse = (content: unknown) => problemsOf(env, files({ '/c.json': JSON.stringify(content) })).join('\n');
    expect(refuse({ clients: [{ name: 'a', token: 'short' }] })).toMatch(/client a of \/c.json: a token is a secret of 16 characters or more/);
    expect(refuse({ clients: [{ token: 'a-token-of-sixteen-chars' }] })).toMatch(/needs a name/);
    expect(refuse({ clients: [{ name: 'a' }] })).toMatch(/needs a token, or a tokenHash/);
    expect(refuse({ clients: [{ name: 'a', token: 'a-token-of-sixteen-chars', maxFilesPerLinks: 3 }] })).toMatch(/maxFilesPerLinks is not a setting of a client/); // a misspelt setting is not ignored
    expect(refuse({ clients: [{ name: 'a', token: 'a-token-of-sixteen-chars', maxFileBytes: 'big' }] })).toMatch(/maxFileBytes is a whole number above 0/);
    expect(refuse({ clients: [] })).toMatch(/holds no clients/);
    expect(problemsOf(env, files({})).join()).toMatch(/CLIENTS_FILE \/c.json cannot be read/);
    expect(problemsOf(env, files({ '/c.json': '{not json' })).join()).toMatch(/CLIENTS_FILE \/c.json cannot be read/);
  });

  it('read an address list, and report a bad one with what it says', () => {
    expect(loadConfig({ ...BASE, MANAGEMENT_ALLOW: '10.0.0.0/8, 203.0.113.7' }).options.managementAllow).toEqual(['10.0.0.0/8', '203.0.113.7']);
    expect(problemsOf({ ...BASE, MANAGEMENT_ALLOW: '10.0.0.0/8, nope' }).join()).toMatch(/MANAGEMENT_ALLOW: nope is not an address or a range/);
    const reader = files({ '/c.json': JSON.stringify({ clients: [{ name: 'a', token: 'a-token-of-sixteen-chars', allow: ['203.0.113.0/24'] }] }) });
    expect(loadConfig({ ...BASE, CLIENTS_FILE: '/c.json' }, reader).options.clients[1]?.allow).toEqual(['203.0.113.0/24']);
    const bad = files({ '/c.json': JSON.stringify({ clients: [{ name: 'a', token: 'a-token-of-sixteen-chars', allow: ['300.1.1.1'] }] }) });
    expect(problemsOf({ ...BASE, CLIENTS_FILE: '/c.json' }, bad).join()).toMatch(/client a of \/c.json: allow: 300.1.1.1 is not an address/);
    const empty = files({ '/c.json': JSON.stringify({ clients: [{ name: 'a', token: 'a-token-of-sixteen-chars', allow: [] }] }) });
    expect(problemsOf({ ...BASE, CLIENTS_FILE: '/c.json' }, empty).join()).toMatch(/allow is a list of addresses and ranges/);
  });

  it('refuse a ticket secret or a metrics token that is too short to be one, once each', () => {
    const problems = problemsOf({ ...BASE, TICKET_SECRET: 'a', METRICS_TOKEN: 'x' });
    expect(problems.filter((problem) => /^TICKET_SECRET is a secret of 16 characters or more/.test(problem))).toHaveLength(1);
    expect(problems.filter((problem) => /^METRICS_TOKEN is a secret of 16 characters or more/.test(problem))).toHaveLength(1);
    const { options } = loadConfig({ ...BASE, TICKET_SECRET: 'a-ticket-secret-of-sixteen', METRICS_TOKEN: 'a-metrics-token-of-sixteen' });
    expect(options.ticketSecret).toBe('a-ticket-secret-of-sixteen');
    expect(options.metricsToken).toBe('a-metrics-token-of-sixteen');
    expect(loadConfig(BASE).options).not.toHaveProperty('metricsToken');
  });

  it('tell a host behind a proxy that every caller looks the same until it says so', () => {
    expect(loadConfig(BASE).notes.join()).toMatch(/set TRUST_PROXY=1/);
    expect(loadConfig({ ...BASE, TRUST_PROXY: '1' }).notes).toEqual([]);
  });

  it('are read in the units they are named in', () => {
    const { options, runtime } = loadConfig({
      ...BASE,
      DEFAULT_LIFETIME_DAYS: '7',
      MAX_LIFETIME_DAYS: '90',
      RETENTION_DAYS: '0',
      SWEEP_INTERVAL_MINUTES: '5',
      RATE_LIMIT_PUBLIC: '0',
      RATE_LIMIT_AUTH_FAILURES: '3',
      ALLOWED_CONTENT_TYPES: 'application/smart-health-card, application/fhir+json',
      LOG_REQUESTS: 'false',
      SHUTDOWN_GRACE_SECONDS: '30',
      HEADERS_TIMEOUT_SECONDS: '5',
      REQUEST_TIMEOUT_SECONDS: '60',
    });
    expect(options.defaultLifetimeSeconds).toBe(7 * 86_400);
    expect(options.maxLifetimeSeconds).toBe(90 * 86_400);
    expect(options.retainSeconds).toBe(0);
    expect(options.sweepIntervalSeconds).toBe(300);
    expect(options.rateLimits?.public).toBe(0); // 0 turns a limit off
    expect(options.rateLimits?.authFailures).toBe(3);
    expect(options.allowedContentTypes).toEqual(['application/smart-health-card', 'application/fhir+json']);
    expect(runtime).toMatchObject({ logRequests: false, shutdownGraceSeconds: 30, headersTimeoutSeconds: 5, requestTimeoutSeconds: 60 });
  });
});

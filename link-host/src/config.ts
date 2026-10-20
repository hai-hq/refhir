// The settings of a host started from the environment, read and checked together so that a mistake
// is reported with every other one and not one at a time. `server.ts` is the only caller.
import { readFileSync } from 'node:fs';

import { DEFAULTS } from './app.js';
import type { Client, HostOptions, RateLimits } from './types.js';
import { addressList } from './util.js';

const DAY = 86_400;
const LOOPBACK = new Set(['127.0.0.1', '::1', 'localhost']);

// What a client of the clients file may set, and what each is.
const CLIENT_FIELDS = {
  name: 'string',
  token: 'string',
  tokenHash: 'string',
  defaultAttempts: 'count',
  maxAttempts: 'count',
  maxFileBytes: 'count',
  maxFilesPerLink: 'count',
  defaultLifetimeDays: 'days',
  maxLifetimeDays: 'days',
  requirePasscode: 'boolean',
  allow: 'addresses',
} as const;

export class ConfigError extends Error {
  constructor(readonly problems: string[]) {
    super(problems.join('\n'));
  }
}

export interface Runtime {
  host: string;
  port: number;
  logRequests: boolean;
  shutdownGraceSeconds: number;
  shutdownDelaySeconds: number;
  headersTimeoutSeconds: number;
  requestTimeoutSeconds: number;
  database:
    | { kind: 'memory' }
    | { kind: 'sqlite'; path: string; migrate: boolean }
    | { kind: 'postgres'; url: string; poolMax: number; waitSeconds: number; migrate: boolean };
}

export interface Config {
  options: Omit<HostOptions, 'store'> & { clients: Client[] };
  runtime: Runtime;
  notes: string[];
}

type Env = Record<string, string | undefined>;

export function loadConfig(env: Env = process.env, { read = (file: string) => readFileSync(file, 'utf8') } = {}): Config {
  const problems: string[] = [];
  const notes: string[] = [];
  const bad = (message: string) => void problems.push(message);

  function number(name: string, fallback: number, { min = 0, max = Number.MAX_SAFE_INTEGER, integer = true } = {}): number {
    const text = env[name];
    if (text === undefined || text === '') return fallback;
    const value = Number(text);
    if (!Number.isFinite(value) || (integer && !Number.isInteger(value)) || value < min || value > max) {
      bad(`${name} is ${integer ? 'a whole number' : 'a number'} from ${min} to ${max}, not "${text}"`);
      return fallback;
    }
    return value;
  }
  function boolean(name: string, fallback: boolean): boolean {
    const text = env[name];
    if (text === undefined || text === '') return fallback;
    if (/^(true|1|yes)$/i.test(text)) return true;
    if (/^(false|0|no)$/i.test(text)) return false;
    bad(`${name} is true or false, not "${text}"`);
    return fallback;
  }
  // A secret that signs or opens something is as long as ADMIN_TOKEN's, or it is refused.
  function secret(name: string): string | undefined {
    const value = env[name];
    if (!value) return undefined;
    if (value.length < 16) {
      bad(`${name} is a secret of 16 characters or more`);
      return undefined;
    }
    return value;
  }
  function list(name: string): string[] | undefined {
    const text = env[name];
    if (text === undefined || text.trim() === '') return undefined;
    return text.split(',').map((part) => part.trim()).filter(Boolean);
  }

  const port = number('PORT', 8090, { min: 1, max: 65535 });
  const host = env.HOST || '0.0.0.0';
  const loopback = LOOPBACK.has(host);

  // Who may make links.
  const clients: Client[] = [];
  if (env.ADMIN_TOKEN) {
    if (env.ADMIN_TOKEN.length < 16) bad('ADMIN_TOKEN is a secret of 16 characters or more');
    else clients.push({ name: 'admin', token: env.ADMIN_TOKEN });
  }
  if (env.CLIENTS_FILE) {
    try {
      clients.push(...readClients(env.CLIENTS_FILE, read, bad));
    } catch (error) {
      bad(`CLIENTS_FILE ${env.CLIENTS_FILE} cannot be read: ${(error as Error).message}`);
    }
  }
  if (!clients.length && !problems.some((problem) => problem.startsWith('ADMIN_TOKEN') || problem.startsWith('CLIENTS_FILE'))) {
    bad('Set ADMIN_TOKEN to a secret of 16 characters or more (or CLIENTS_FILE to a file of clients): it is what lets a caller make links.');
  }

  // The address links are made for.
  let publicUrl = env.PUBLIC_URL;
  if (!publicUrl) {
    if (loopback) publicUrl = `http://localhost:${port}`;
    else bad('Set PUBLIC_URL to the address this host is reached at, such as https://links.example.org: every link it makes points there.');
  } else {
    try {
      const parsed = new URL(publicUrl);
      const local = LOOPBACK.has(parsed.hostname) || parsed.hostname.endsWith('.localhost');
      if (!/^https?:$/.test(parsed.protocol) || parsed.search || parsed.hash) {
        bad('PUBLIC_URL is an http or https address with no query or fragment');
      } else if (parsed.protocol !== 'https:' && !local) {
        bad('PUBLIC_URL has to be https: a receiver will not take a link to an address that is not');
      }
      publicUrl = publicUrl.replace(/\/+$/, '');
    } catch {
      bad(`PUBLIC_URL is not an address: ${publicUrl}`);
    }
  }

  // Where links are kept: the kind of address in DATABASE_URL says which database.
  let database: Runtime['database'] = { kind: 'memory' };
  const url = env.DATABASE_URL;
  const asked = env.STORE;
  if (asked && !['memory', 'postgres', 'sqlite'].includes(asked)) {
    bad(`STORE is memory, postgres or sqlite, not "${asked}"`);
  } else if (asked === 'memory') {
    notes.push('STORE=memory: links are kept in memory and lost when the host stops. That is for trying it out.');
  } else if (!url) {
    bad('Set DATABASE_URL to where the host keeps links: postgres://user:password@host:5432/links, or sqlite:/data/links.db (or STORE=memory to try it out).');
  } else if (/^sqlite:/i.test(url)) {
    const file = url.replace(/^sqlite:(\/\/)?/i, '');
    if (!file) bad('DATABASE_URL is sqlite: and the path of a file, such as sqlite:/data/links.db');
    else if (asked === 'postgres') bad('STORE is postgres, and DATABASE_URL is a SQLite file');
    else database = { kind: 'sqlite', path: file, migrate: boolean('AUTO_MIGRATE', true) };
    if (file === ':memory:') notes.push('DATABASE_URL is sqlite::memory:, which is lost when the host stops. That is for trying it out.');
  } else {
    try {
      const parsed = new URL(url);
      if (!/^postgres(ql)?:$/.test(parsed.protocol)) throw new Error('not postgres');
    } catch {
      bad('DATABASE_URL is postgres://user:password@host:5432/database, or sqlite: and the path of a file');
    }
    if (asked === 'sqlite') bad('STORE is sqlite, and DATABASE_URL is a Postgres address');
    database = {
      kind: 'postgres',
      url,
      poolMax: number('DATABASE_POOL_MAX', 10, { min: 1, max: 200 }),
      waitSeconds: number('DATABASE_WAIT_SECONDS', 30, { min: 0, max: 600 }),
      migrate: boolean('AUTO_MIGRATE', true),
    };
  }

  const trustProxy = number('TRUST_PROXY', DEFAULTS.trustProxy, { min: 0, max: 10 });
  if (!trustProxy && publicUrl?.startsWith('https:')) {
    notes.push(
      'PUBLIC_URL is https and the host does not serve HTTPS itself, so a proxy is taking it: set TRUST_PROXY=1 or every caller is counted as the proxy, and shares one rate limit.',
    );
  }

  const ticketSecret = secret('TICKET_SECRET');
  const metricsToken = secret('METRICS_TOKEN');

  const options: Config['options'] = {
    clients,
    ...(publicUrl ? { publicUrl } : {}),
    trustProxy,
    ...(ticketSecret ? { ticketSecret } : {}),
    ...(metricsToken ? { metricsToken } : {}),
    corsOrigins: list('CORS_ORIGINS') ?? ['*'],
    managementCorsOrigins: list('MANAGEMENT_CORS_ORIGINS') ?? [],
    ...(list('MANAGEMENT_ALLOW') ? { managementAllow: list('MANAGEMENT_ALLOW')! } : {}),
    ...(list('ALLOWED_CONTENT_TYPES') ? { allowedContentTypes: list('ALLOWED_CONTENT_TYPES')! } : {}),
    requirePasscode: boolean('REQUIRE_PASSCODE', false),
    recordClientInfo: boolean('RECORD_CLIENT_INFO', true),
    defaultAttempts: number('DEFAULT_ATTEMPTS', DEFAULTS.defaultAttempts, { min: 1, max: 20 }),
    maxAttempts: number('MAX_ATTEMPTS', DEFAULTS.maxAttempts, { min: 1, max: 20 }),
    maxFileBytes: number('MAX_FILE_BYTES', DEFAULTS.maxFileBytes, { min: 1024, max: 1024 * 1024 * 1024 }),
    maxFilesPerLink: number('MAX_FILES_PER_LINK', DEFAULTS.maxFilesPerLink, { min: 1, max: 1000 }),
    defaultLifetimeSeconds: number('DEFAULT_LIFETIME_DAYS', DEFAULTS.defaultLifetimeSeconds / DAY, { min: 1, max: 36500 }) * DAY,
    maxLifetimeSeconds: number('MAX_LIFETIME_DAYS', DEFAULTS.maxLifetimeSeconds / DAY, { min: 1, max: 36500 }) * DAY,
    retainSeconds: number('RETENTION_DAYS', DEFAULTS.retainSeconds / DAY, { min: 0, max: 36500 }) * DAY,
    sweepIntervalSeconds: number('SWEEP_INTERVAL_MINUTES', DEFAULTS.sweepIntervalSeconds / 60, { min: 0, max: 1440 }) * 60,
    maxConcurrent: number('MAX_CONCURRENT', DEFAULTS.maxConcurrent, { min: 1, max: 100000 }),
    rateLimits: {
      public: number('RATE_LIMIT_PUBLIC', DEFAULTS.rateLimits.public),
      perLink: number('RATE_LIMIT_PER_LINK', DEFAULTS.rateLimits.perLink),
      management: number('RATE_LIMIT_MANAGEMENT', DEFAULTS.rateLimits.management),
      create: number('RATE_LIMIT_CREATE', DEFAULTS.rateLimits.create),
      authFailures: number('RATE_LIMIT_AUTH_FAILURES', DEFAULTS.rateLimits.authFailures),
    } satisfies RateLimits,
  };
  if (options.managementAllow) {
    try {
      addressList(options.managementAllow, 'MANAGEMENT_ALLOW');
    } catch (error) {
      bad((error as Error).message);
    }
  }
  if ((options.defaultAttempts ?? 0) > (options.maxAttempts ?? 0)) bad('DEFAULT_ATTEMPTS is more than MAX_ATTEMPTS');
  if ((options.defaultLifetimeSeconds ?? 0) > (options.maxLifetimeSeconds ?? 0)) bad('DEFAULT_LIFETIME_DAYS is more than MAX_LIFETIME_DAYS');
  if (options.allowedContentTypes?.some((type) => !/^[\w.+-]+\/[\w.+-]+$/.test(type))) {
    bad('ALLOWED_CONTENT_TYPES is a comma-separated list of media types, such as application/fhir+json');
  }

  const runtime: Runtime = {
    host,
    port,
    logRequests: boolean('LOG_REQUESTS', true),
    shutdownGraceSeconds: number('SHUTDOWN_GRACE_SECONDS', 10, { min: 0, max: 600 }),
    shutdownDelaySeconds: number('SHUTDOWN_DELAY_SECONDS', 0, { min: 0, max: 600 }),
    headersTimeoutSeconds: number('HEADERS_TIMEOUT_SECONDS', 10, { min: 1, max: 600 }),
    requestTimeoutSeconds: number('REQUEST_TIMEOUT_SECONDS', 120, { min: 1, max: 3600 }),
    database,
  };
  if (problems.length) throw new ConfigError(problems);
  return { options, runtime, notes };
}

// The clients file is `{ "clients": [ ... ] }` or just the list.
function readClients(file: string, read: (file: string) => string, bad: (message: string) => void): Client[] {
  const parsed: unknown = JSON.parse(read(file));
  const entries = Array.isArray(parsed) ? parsed : (parsed as { clients?: unknown })?.clients;
  if (!Array.isArray(entries) || !entries.length) throw new Error('it holds no clients');
  return entries.flatMap((entry: unknown, index): Client[] => {
    const record = (entry ?? {}) as Record<string, unknown>;
    const label = `client ${typeof record.name === 'string' ? record.name : index + 1} of ${file}`;
    if (!entry || typeof entry !== 'object') {
      bad(`${label} is not an object`);
      return [];
    }
    let ok = true;
    for (const [field, value] of Object.entries(record)) {
      const type = (CLIENT_FIELDS as Record<string, string>)[field];
      const fine =
        type === 'string' ? typeof value === 'string' && value !== '' :
        type === 'boolean' ? typeof value === 'boolean' :
        type === 'count' ? Number.isInteger(value) && (value as number) > 0 :
        type === 'days' ? Number.isInteger(value) && (value as number) > 0 && (value as number) <= 36500 :
        type === 'addresses' ? Array.isArray(value) && value.length > 0 && value.every((item) => typeof item === 'string') :
        undefined;
      if (fine === undefined) {
        bad(`${label}: ${field} is not a setting of a client (${Object.keys(CLIENT_FIELDS).join(', ')})`);
        ok = false;
      } else if (!fine) {
        const what = { days: 'whole number of days', count: 'whole number above 0', addresses: 'list of addresses and ranges' }[type as string] ?? type;
        bad(`${label}: ${field} is a ${what}`);
        ok = false;
      } else if (type === 'addresses') {
        try {
          addressList(value as string[], `${label}: allow`);
        } catch (error) {
          bad((error as Error).message);
          ok = false;
        }
      }
    }
    if (!record.name) {
      bad(`${label} needs a name, which is what the logs say it did`);
      ok = false;
    }
    if (!record.token && !record.tokenHash) {
      bad(`${label} needs a token, or a tokenHash made by "refhir-link-host new-token"`);
      ok = false;
    }
    if (typeof record.token === 'string' && record.token.length < 16) {
      bad(`${label}: a token is a secret of 16 characters or more`);
      ok = false;
    }
    if (!ok) return [];
    const { defaultLifetimeDays, maxLifetimeDays, ...rest } = record as Record<string, unknown> & { defaultLifetimeDays?: number; maxLifetimeDays?: number };
    return [
      {
        ...(rest as unknown as Client),
        ...(defaultLifetimeDays ? { defaultLifetimeSeconds: defaultLifetimeDays * DAY } : {}),
        ...(maxLifetimeDays ? { maxLifetimeSeconds: maxLifetimeDays * DAY } : {}),
      },
    ];
  });
}

// A SMART Health Links host: it stores encrypted files and answers whoever opens a link.
//
// It follows the manifest protocol of the SMART Health Links specification. It never has a key: a
// file arrives encrypted, is stored as it came, and is handed out as it is, so a host that is
// broken into holds nothing that can be read.
//
// `createHost(options)` returns a Hono app, which is a function from a web `Request` to a `Response`,
// so it runs under Node (`server.ts` does that), Bun, Deno or anything else that speaks the web
// standard, given a store. Where links are kept is a `Store` (see `types.ts`); everything that reads
// and then changes one link does it inside `store.update`, which is a transaction on that link, and
// is what keeps two requests from counting the same passcode guess once.
import { createHmac, randomBytes, scrypt, timingSafeEqual } from 'node:crypto';
import { promisify } from 'node:util';

import { getConnInfo } from '@hono/node-server/conninfo';
import { Hono, type Context, type MiddlewareHandler } from 'hono';

import { rateLimiter, type Verdict as Limit } from './limits.js';
import { createMetrics } from './metrics.js';
import { isLive } from './store/shared.js';
import {
  StoreBusy,
  type AccessEntry,
  type Client,
  type HostEvent,
  type HostOptions,
  type Link,
  type LinkSummary,
  type LinkTx,
  type RateLimits,
  type Store,
  type SweepCounts,
  type Verdict,
} from './types.js';
import { addressList, base64url, clientAddress, same, sha256 } from './util.js';

const scryptAsync = promisify(scrypt) as (password: string, salt: Buffer, keylen: number) => Promise<Buffer>;

const FILE_ID = /^f\d{1,6}$/;
const CONTENT_TYPE = /^[\w.+-]+\/[\w.+-]+(;[\w.+=\- ]+)*$/;
const TICKET_SECONDS = 60;
const JSON_LIMIT = 4096;
const DAY = 86_400;

/** What a host does when it is not told otherwise. */
export const DEFAULTS = Object.freeze({
  defaultAttempts: 5,
  maxAttempts: 20,
  maxFileBytes: 10 * 1024 * 1024,
  maxFilesPerLink: 10,
  defaultLifetimeSeconds: 30 * DAY,
  maxLifetimeSeconds: 365 * DAY,
  retainSeconds: 30 * DAY,
  sweepIntervalSeconds: 600,
  accessKept: 100,
  maxConcurrent: 200,
  trustProxy: 0,
  rateLimits: Object.freeze({ public: 120, perLink: 60, management: 300, create: 60, authFailures: 10 }) as RateLimits,
});

type Kind = 'public' | 'management' | 'ops';
interface Ctx {
  requestId: string;
  ip: string;
  route: string;
  kind: Kind;
  client?: string;
  link?: string;
}
type Env = { Variables: { ctx: Ctx } };
type C = Context<Env>;

export class Refused extends Error {
  constructor(
    readonly status: number,
    readonly body: unknown,
    readonly headers: Record<string, string> = {},
  ) {
    super(`${status}`);
  }
}
const notFound = () => new Refused(404, { error: 'not found' });
const notYours = (link: Link | null) => new Refused(link ? 401 : 404, { error: 'not found, or not yours' });
const tooMany = (limit: Limit) => new Refused(429, { error: 'too many requests' }, { 'retry-after': String(limit.retryAfter) });

function reply(status: number, body?: unknown, type = 'application/json', extra: Record<string, string> = {}): Response {
  const payload = body === undefined ? null : typeof body === 'string' ? body : JSON.stringify(body);
  const headers = new Headers(extra);
  if (payload !== null) headers.set('content-type', type);
  return new Response(payload, { status, headers });
}

// A compact JWE that is direct and A256GCM, as the specification asks of a file. The host cannot
// open it; this only keeps a file that was never encrypted from being stored by mistake.
function looksEncrypted(text: string): boolean {
  const parts = text.split('.');
  if (parts.length !== 5 || parts[1] !== '') return false;
  if (parts.some((part, index) => index !== 1 && !/^[A-Za-z0-9_-]+$/.test(part))) return false;
  try {
    const header = JSON.parse(Buffer.from(parts[0]!, 'base64url').toString('utf8')) as { alg?: string; enc?: string };
    return header.alg === 'dir' && header.enc === 'A256GCM';
  } catch {
    return false;
  }
}

interface KnownClient {
  client: Client;
  hash: Buffer;
  allows: ((ip: string) => boolean) | null;
}

function knownClients({ clients = [], adminToken }: Pick<HostOptions, 'clients' | 'adminToken'>): KnownClient[] {
  const list = adminToken ? [...clients, { name: 'admin', token: adminToken }] : [...clients];
  const names = new Set<string>();
  return list.map((client, index) => {
    const name = client.name ?? `client-${index + 1}`;
    if (names.has(name)) throw new Error(`two clients are named ${name}`);
    names.add(name);
    let hash: Buffer;
    if (client.tokenHash) {
      hash = Buffer.from(String(client.tokenHash).replace(/^sha256:/, ''), 'hex');
      if (hash.length !== 32) throw new Error(`client ${name}: tokenHash is the SHA-256 of the token, in hex`);
    } else {
      if (typeof client.token !== 'string' || client.token.length < 16) {
        throw new Error(`client ${name}: a token is a secret of 16 characters or more`);
      }
      hash = sha256(client.token);
    }
    return { client: { ...client, name }, hash, allows: client.allow ? addressList(client.allow, `client ${name}: allow`) : null };
  });
}

/** What is a route, which is what each is called in the logs and the counts, and which of the three
 * kinds it is: what a receiver calls, what the owner of a link calls, and what the operator calls. */
const ID = ':id{[A-Za-z0-9_-]{43}}';
const ROUTES = [
  { method: 'GET', path: '/healthz', route: 'healthz', kind: 'ops' },
  { method: 'GET', path: '/readyz', route: 'readyz', kind: 'ops' },
  { method: 'GET', path: '/metrics', route: 'metrics', kind: 'ops' },
  { method: 'POST', path: '/shl', route: 'create', kind: 'management' },
  { method: 'POST', path: `/shl/${ID}`, route: 'manifest', kind: 'public' },
  { method: 'GET', path: `/shl/${ID}`, route: 'direct', kind: 'public' },
  { method: 'DELETE', path: `/shl/${ID}`, route: 'switch-off', kind: 'management' },
  { method: 'POST', path: `/shl/${ID}/file`, route: 'upload', kind: 'management' },
  { method: 'GET', path: `/shl/${ID}/access`, route: 'access', kind: 'management' },
  { method: 'GET', path: `/shl/${ID}/file/:file`, route: 'file', kind: 'public' },
] as const satisfies ReadonlyArray<{ method: string; path: string; route: string; kind: Kind }>;

const SHAPES: Array<[string, RegExp]> = [
  ['/healthz', /^\/healthz$/],
  ['/readyz', /^\/readyz$/],
  ['/metrics', /^\/metrics$/],
  ['/shl', /^\/shl$/],
  [`/shl/${ID}`, /^\/shl\/[A-Za-z0-9_-]{43}$/],
  [`/shl/${ID}/file`, /^\/shl\/[A-Za-z0-9_-]{43}\/file$/],
  [`/shl/${ID}/access`, /^\/shl\/[A-Za-z0-9_-]{43}\/access$/],
  [`/shl/${ID}/file/:file`, /^\/shl\/[A-Za-z0-9_-]{43}\/file\/[^/]+$/],
];

/** The kind of request a browser is asking whether it may make: from the path and the method. */
function kindAsked(method: string, path: string): Kind {
  const shape = SHAPES.find(([, pattern]) => pattern.test(path))?.[0];
  return ROUTES.find((entry) => entry.path === shape && entry.method === method)?.kind ?? 'public';
}

export interface Host {
  /** The app. Give `app.fetch` to a server. */
  app: Hono<Env>;
  store: Store;
  /** Deletes what has ended, once. The host does it by itself every `sweepIntervalSeconds`.
   * `null` when another process was already doing it. */
  sweep(): Promise<SweepCounts | null>;
  /** Starts refusing new work and says not ready, so a balancer sends it elsewhere while it ends. */
  drain(): void;
  /** Stops what the host does in the background. It does not close the store. */
  close(): void;
}

export async function createHost(options: HostOptions): Promise<Host> {
  const {
    store,
    publicUrl,
    now = () => Date.now(),
    defaultAttempts = DEFAULTS.defaultAttempts,
    maxAttempts = DEFAULTS.maxAttempts,
    maxFileBytes = DEFAULTS.maxFileBytes,
    maxFilesPerLink = DEFAULTS.maxFilesPerLink,
    defaultLifetimeSeconds = DEFAULTS.defaultLifetimeSeconds,
    maxLifetimeSeconds = DEFAULTS.maxLifetimeSeconds,
    retainSeconds = DEFAULTS.retainSeconds,
    sweepIntervalSeconds = DEFAULTS.sweepIntervalSeconds,
    accessKept = DEFAULTS.accessKept,
    maxConcurrent = DEFAULTS.maxConcurrent,
    trustProxy = DEFAULTS.trustProxy,
    requirePasscode = false,
    allowedContentTypes,
    corsOrigins = ['*'],
    managementCorsOrigins = [],
    managementAllow,
    recordClientInfo = true,
    metricsToken,
    authenticate,
    authorizeCreate,
    authorizeManifest,
    onEvent,
    logger,
  } = options;
  const rateLimits = { ...DEFAULTS.rateLimits, ...options.rateLimits };
  const clients = knownClients(options);
  if (!clients.length && !authenticate) {
    throw new Error('createHost needs a way to tell who may make links: adminToken, clients or authenticate');
  }
  const ticketSecret = options.ticketSecret ?? (await store.secret('ticket'));
  const allowedTypes = allowedContentTypes?.map((type) => type.toLowerCase());
  const managementAllows = managementAllow?.length ? addressList(managementAllow, 'managementAllow') : null;
  const https = /^https:/i.test(publicUrl ?? '');

  const seconds = () => Math.floor(now() / 1000);
  const live = (link: Link | null): link is Link => isLive(link, seconds());
  const base = (c: C) => (publicUrl ? publicUrl.replace(/\/+$/, '') : `http://${c.req.header('host')}`);

  const metrics = createMetrics();
  const limiters = {
    public: rateLimiter({ limit: rateLimits.public, now }),
    perLink: rateLimiter({ limit: rateLimits.perLink, now }),
    management: rateLimiter({ limit: rateLimits.management, now }),
    create: rateLimiter({ limit: rateLimits.create, now }),
    authFailures: rateLimiter({ limit: rateLimits.authFailures, windowSeconds: 300, now }),
  };
  const limited = (bucket: string, limit: Limit) => {
    if (limit.ok) return;
    metrics.count('shl_rate_limited_total', { bucket });
    throw tooMany(limit);
  };

  // What is logged says which link by its first eight characters, which is enough to follow one
  // through the logs and no use for opening it: a link's address is the capability.
  function log(level: 'info' | 'warn' | 'error', msg: string, fields: Record<string, unknown> = {}) {
    const { id, ...rest } = fields;
    const entry = { ts: new Date(now()).toISOString(), level, msg, ...(typeof id === 'string' ? { link: id.slice(0, 8) } : {}), ...rest };
    if (logger) logger(entry);
    else if (level === 'error') console.error(msg, rest.error ?? '');
  }
  const failure = (error: unknown) => String((error as Error)?.stack ?? error);
  function emit(type: string, fields: Record<string, unknown> = {}) {
    const event: HostEvent = { type, at: seconds(), ...fields };
    log('info', type, fields);
    if (!onEvent) return;
    try {
      void Promise.resolve(onEvent(event)).catch((error) => log('error', 'onEvent failed', { error: failure(error) }));
    } catch (error) {
      log('error', 'onEvent failed', { error: failure(error) });
    }
  }

  const bearer = (c: C) => (c.req.header('authorization') ?? '').replace(/^Bearer /i, '');
  const manages = (link: Link | null, c: C): link is Link =>
    link !== null && timingSafeEqual(sha256(bearer(c)), Buffer.from(link.tokenHash, 'hex'));

  async function who(c: C, ctx: Ctx): Promise<Client> {
    limited('auth-failures', limiters.authFailures.blocked(ctx.ip));
    let known: { client: Client; allows: ((ip: string) => boolean) | null } | null = null;
    if (authenticate) {
      const client = await authenticate(c.req.raw, { ip: ctx.ip });
      if (client) known = { client, allows: client.allow ? addressList(client.allow, `client ${client.name}: allow`) : null };
    } else {
      // Every client is compared, so the time taken does not say which one matched or how far.
      const presented = sha256(bearer(c));
      for (const candidate of clients) if (timingSafeEqual(presented, candidate.hash)) known ??= candidate;
    }
    if (!known) {
      limiters.authFailures.hit(ctx.ip);
      throw new Refused(401, { error: 'a token that may make links is required' });
    }
    ctx.client = known.client.name ?? 'client';
    // A token that is right, from an address its client does not use, is told so and not counted as
    // a guess: it is the client's own mistake, or a stolen token that has been found out.
    if (known.allows && !known.allows(ctx.ip)) {
      emit('client.refused', { client: ctx.client, ip: ctx.ip, reason: 'address' });
      throw new Refused(403, { error: 'this token is not accepted from this address' });
    }
    return known.client;
  }

  function requireManager(c: C, ctx: Ctx, link: Link | null): asserts link is Link {
    limited('auth-failures', limiters.authFailures.blocked(ctx.ip));
    if (!manages(link, c)) {
      if (link) limiters.authFailures.hit(ctx.ip);
      throw notYours(link);
    }
  }

  async function permitted<D>(hook: ((details: D) => Verdict | Promise<Verdict>) | undefined, details: D, status = 403) {
    if (!hook) return;
    const verdict = await hook(details);
    if (verdict === false) throw new Refused(status, { error: 'not allowed' });
    if (verdict && typeof verdict === 'object' && verdict.allow === false) {
      throw new Refused(verdict.status ?? status, { error: verdict.error ?? 'not allowed' });
    }
  }

  async function readBody(request: Request, limit: number): Promise<string> {
    const tooLarge = () => new Refused(413, { error: 'the request is too large' }, { connection: 'close' });
    const declared = Number(request.headers.get('content-length'));
    if (Number.isFinite(declared) && declared > limit) throw tooLarge();
    if (!request.body) return '';
    const reader = request.body.getReader();
    const chunks: Uint8Array[] = [];
    let size = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.length;
      if (size > limit) {
        await reader.cancel().catch(() => undefined);
        throw tooLarge();
      }
      chunks.push(value);
    }
    return Buffer.concat(chunks).toString('utf8');
  }
  async function readJson(c: C): Promise<Record<string, unknown>> {
    const text = await readBody(c.req.raw, JSON_LIMIT);
    try {
      const value: unknown = text ? JSON.parse(text) : {};
      if (value && typeof value === 'object' && !Array.isArray(value)) return value as Record<string, unknown>;
    } catch {
      // falls through to the refusal
    }
    throw new Refused(400, { error: 'the body has to be a JSON object' });
  }

  const macOf = (id: string, fileId: string, expires: number | string) =>
    createHmac('sha256', ticketSecret).update(`${id}:${fileId}:${expires}`).digest();
  function ticketFor(id: string, fileId: string): string {
    const expires = seconds() + TICKET_SECONDS;
    return `${expires}.${base64url(macOf(id, fileId, expires))}`;
  }
  function ticketHolds(id: string, fileId: string, ticket: string | undefined): boolean {
    const [expires, mac] = (ticket ?? '').split('.');
    if (!/^\d+$/.test(expires ?? '') || !mac || Number(expires) < seconds()) return false;
    const wanted = macOf(id, fileId, expires!);
    const given = Buffer.from(mac, 'base64url');
    return given.length === wanted.length && timingSafeEqual(given, wanted);
  }

  const policyOf = (client: Client) => {
    const maxLifetime = client.maxLifetimeSeconds ?? maxLifetimeSeconds;
    return {
      defaultAttempts: Math.min(client.defaultAttempts ?? defaultAttempts, client.maxAttempts ?? maxAttempts),
      maxAttempts: client.maxAttempts ?? maxAttempts,
      // A client whose longest link is three days does not get thirty because that is the host's usual.
      defaultLifetimeSeconds: Math.min(client.defaultLifetimeSeconds ?? defaultLifetimeSeconds, maxLifetime),
      maxLifetimeSeconds: maxLifetime,
      maxFileBytes: client.maxFileBytes ?? maxFileBytes,
      maxFilesPerLink: client.maxFilesPerLink ?? maxFilesPerLink,
      requirePasscode: client.requirePasscode ?? requirePasscode,
    };
  };
  // What a hook is told of a link: nothing that opens it.
  const describe = (link: Link): LinkSummary => ({
    id: link.id,
    createdBy: link.createdBy,
    createdAt: link.createdAt,
    expiresAt: link.expiresAt,
    passcodeRequired: Boolean(link.passcode),
    files: link.files.length,
  });

  async function makeLink(c: C, ctx: Ctx): Promise<Response> {
    const client = await who(c, ctx);
    limited('create', limiters.create.hit(ctx.client ?? 'client'));
    const asked = await readJson(c);
    const policy = policyOf(client);
    const attempts = asked.attempts ?? policy.defaultAttempts;
    if (typeof attempts !== 'number' || !Number.isInteger(attempts) || attempts < 1 || attempts > policy.maxAttempts) {
      throw new Refused(400, { error: `attempts is a whole number from 1 to ${policy.maxAttempts}` });
    }
    const passcode = asked.passcode;
    if (passcode !== undefined && (typeof passcode !== 'string' || !passcode || passcode.length > 64)) {
      throw new Refused(400, { error: 'passcode is a string of 1 to 64 characters' });
    }
    if (policy.requirePasscode && !passcode) {
      throw new Refused(400, { error: 'this host gives every link a passcode: say one' });
    }
    const expiresAt = asked.expiresAt;
    if (expiresAt !== undefined && (typeof expiresAt !== 'number' || !Number.isInteger(expiresAt) || expiresAt <= seconds())) {
      throw new Refused(400, { error: 'expiresAt is a time in the future, in seconds since 1970' });
    }
    const latest = seconds() + policy.maxLifetimeSeconds;
    if (typeof expiresAt === 'number' && expiresAt > latest) {
      throw new Refused(400, { error: `expiresAt is later than this host keeps a link: ${latest} at the latest` });
    }
    await permitted(authorizeCreate, { client: client.name, options: asked, ip: ctx.ip, request: c.req.raw });

    const id = base64url(randomBytes(32));
    const manageToken = base64url(randomBytes(32));
    const link: Link = {
      id,
      tokenHash: sha256(manageToken).toString('hex'),
      active: true,
      createdAt: seconds(),
      createdBy: client.name,
      expiresAt: typeof expiresAt === 'number' ? expiresAt : seconds() + policy.defaultLifetimeSeconds,
      endedAt: null,
      attemptsLeft: attempts,
      passcode: null,
      maxFiles: policy.maxFilesPerLink,
      maxFileBytes: policy.maxFileBytes,
      files: [],
    };
    if (typeof passcode === 'string' && passcode) {
      const salt = randomBytes(16);
      link.passcode = { salt: salt.toString('hex'), hash: (await scryptAsync(passcode, salt, 32)).toString('hex') };
    }
    await store.create(link);
    metrics.count('shl_links_created_total');
    ctx.link = id;
    emit('link.created', { id, client: client.name, expiresAt: link.expiresAt, passcode: Boolean(link.passcode) });
    return reply(201, {
      id,
      manifestUrl: `${base(c)}/shl/${id}`,
      manageToken,
      expiresAt: link.expiresAt,
      passcodeRequired: Boolean(link.passcode),
    });
  }

  async function putFile(c: C, ctx: Ctx): Promise<Response> {
    const id = c.req.param('id')!;
    ctx.link = id;
    const first = await store.get(id);
    if (!live(first)) throw notYours(null);
    requireManager(c, ctx, first);
    const contentType = c.req.query('contentType') ?? 'application/fhir+json';
    if (!CONTENT_TYPE.test(contentType)) throw new Refused(400, { error: 'contentType is not a media type' });
    if (allowedTypes && !allowedTypes.includes(contentType.split(';')[0]!.trim().toLowerCase())) {
      throw new Refused(400, { error: `this host keeps files of these types only: ${allowedTypes.join(', ')}` });
    }
    const full = () => new Refused(409, { error: `a link holds up to ${first.maxFiles} files` });
    if (first.files.length >= first.maxFiles) throw full();
    // The file is read before the link is taken, so a slow upload cannot hold up a manifest.
    const text = (await readBody(c.req.raw, first.maxFileBytes)).trim();
    if (!looksEncrypted(text)) {
      throw new Refused(400, {
        error: 'a file has to be a compact JWE (alg dir, enc A256GCM): this host stores only encrypted files',
      });
    }
    const outcome = await store.update(id, async (link, tx) => {
      if (!live(link)) return { refused: notYours(null) };
      if (!manages(link, c)) return { refused: notYours(link) };
      if (link.files.length >= link.maxFiles) return { refused: full() };
      const fileId = `f${link.files.length}`;
      await tx.putFile({ id: fileId, contentType, bytes: text.length }, text);
      return { fileId };
    });
    if (outcome.refused) throw outcome.refused;
    metrics.count('shl_files_stored_total');
    emit('file.stored', { id, fileId: outcome.fileId, bytes: text.length });
    return reply(201, { fileId: outcome.fileId });
  }

  function record(link: Link, tx: LinkTx, ctx: Ctx, c: C, recipient: string, outcome: AccessEntry['outcome']) {
    const entry: AccessEntry = { recipient, at: seconds(), outcome };
    if (recordClientInfo) {
      entry.ip = ctx.ip;
      entry.userAgent = (c.req.header('user-agent') ?? '').slice(0, 200);
    }
    void link;
    return tx.recordAccess(entry, accessKept);
  }

  // A change may be run more than once (see `Store.update`), so it only changes the link and uses `tx`:
  // what is counted and told is done after, once.
  async function switchOff(link: Link, tx: LinkTx, reason: string): Promise<string> {
    link.active = false;
    link.endedAt = seconds();
    await tx.removeFiles();
    return reason;
  }

  // The manifest, or with `direct` the file itself (the U flag), for whoever has the address.
  async function open(c: C, ctx: Ctx, id: string, who: { recipient: string; passcode?: unknown; direct?: boolean }) {
    ctx.link = id;
    const seen = await store.get(id);
    if (!live(seen) || (who.direct && (seen.passcode || seen.files.length !== 1))) {
      metrics.count('shl_manifest_total', { outcome: 'not_found' });
      throw notFound();
    }
    try {
      await permitted(authorizeManifest, { link: describe(seen), recipient: who.recipient, ip: ctx.ip, request: c.req.raw });
    } catch (error) {
      metrics.count('shl_manifest_total', { outcome: 'denied' });
      emit('manifest.denied', { id, recipient: who.recipient, ip: ctx.ip, reason: 'recipient' });
      throw error;
    }
    type Opened =
      | { result: 'opened'; files: Link['files'] }
      | { result: 'not_found' | 'passcode_required' | 'wrong_passcode'; refused: Refused; ended?: string };
    const outcome = await store.update<Opened>(id, async (link, tx) => {
      if (!live(link)) return { refused: notFound(), result: 'not_found' };
      if (link.passcode && !who.direct) {
        // A request with no passcode is not a guess, so it does not use an attempt up.
        if (typeof who.passcode !== 'string' || !who.passcode) {
          return { refused: new Refused(401, { remainingAttempts: link.attemptsLeft }), result: 'passcode_required' };
        }
        const given = await scryptAsync(who.passcode, Buffer.from(link.passcode.salt, 'hex'), 32);
        if (!timingSafeEqual(given, Buffer.from(link.passcode.hash, 'hex'))) {
          link.attemptsLeft -= 1;
          await record(link, tx, ctx, c, who.recipient, 'wrong-passcode');
          const ended = link.attemptsLeft <= 0 ? await switchOff(link, tx, 'attempts') : undefined;
          return {
            refused: new Refused(401, { remainingAttempts: Math.max(link.attemptsLeft, 0) }),
            result: 'wrong_passcode',
            ...(ended ? { ended } : {}),
          };
        }
      }
      await record(link, tx, ctx, c, who.recipient, 'opened');
      return { result: 'opened', files: link.files.map((file) => ({ ...file })) };
    });
    metrics.count('shl_manifest_total', { outcome: outcome.result });
    if (outcome.result !== 'opened') {
      if (outcome.result === 'wrong_passcode') {
        emit('manifest.denied', { id, recipient: who.recipient, ip: ctx.ip, reason: 'passcode' });
        if (outcome.ended) {
          metrics.count('shl_links_ended_total', { reason: outcome.ended });
          emit('link.ended', { id, reason: outcome.ended });
        }
      }
      throw outcome.refused;
    }
    emit('manifest.opened', { id, recipient: who.recipient, ip: ctx.ip });
    return outcome.files;
  }

  async function manifest(c: C, ctx: Ctx): Promise<Response> {
    const id = c.req.param('id')!;
    const asked = await readJson(c);
    const recipient = asked.recipient;
    if (typeof recipient !== 'string' || !recipient || recipient.length > 200) {
      throw new Refused(400, { error: 'recipient is required: who is asking' });
    }
    limited('link', limiters.perLink.hit(id));
    const files = await open(c, ctx, id, { recipient, passcode: asked.passcode });
    const embeddedLengthMax = asked.embeddedLengthMax;
    const listed: Array<{ contentType: string; embedded?: string; location?: string }> = [];
    for (const file of files) {
      const entry: { contentType: string; embedded?: string; location?: string } = { contentType: file.contentType };
      if (typeof embeddedLengthMax === 'number' && Number.isInteger(embeddedLengthMax) && file.bytes <= embeddedLengthMax) {
        const text = await store.getFile(id, file.id);
        if (text === null) throw notFound();
        entry.embedded = text;
      } else {
        entry.location = `${base(c)}/shl/${id}/file/${file.id}?ticket=${ticketFor(id, file.id)}`;
      }
      listed.push(entry);
    }
    return reply(200, { files: listed });
  }

  async function direct(c: C, ctx: Ctx): Promise<Response> {
    // The U flag: the address is the file. It cannot have a passcode, and has to say who asks.
    const id = c.req.param('id')!;
    const recipient = c.req.query('recipient');
    if (!recipient || recipient.length > 200) throw new Refused(400, { error: 'recipient is required: who is asking' });
    limited('link', limiters.perLink.hit(id));
    const [file] = await open(c, ctx, id, { recipient, direct: true });
    const text = file ? await store.getFile(id, file.id) : null;
    if (text === null) throw notFound();
    return reply(200, text, 'application/jose');
  }

  async function switchOffByOwner(c: C, ctx: Ctx): Promise<Response> {
    const id = c.req.param('id')!;
    ctx.link = id;
    requireManager(c, ctx, await store.get(id));
    type Ended = { refused: Refused } | { ended?: string };
    const outcome = await store.update<Ended>(id, async (link, tx) => {
      if (!manages(link, c)) return { refused: notYours(link) };
      if (!link.active) return {};
      return { ended: await switchOff(link, tx, 'revoked') };
    });
    if ('refused' in outcome) throw outcome.refused;
    if (outcome.ended) {
      metrics.count('shl_links_ended_total', { reason: outcome.ended });
      emit('link.ended', { id, reason: outcome.ended });
    }
    return reply(204);
  }

  async function accessList(c: C, ctx: Ctx): Promise<Response> {
    const id = c.req.param('id')!;
    ctx.link = id;
    const link = await store.get(id);
    requireManager(c, ctx, link);
    return reply(200, { access: await store.listAccess(id), expiresAt: link.expiresAt, active: live(link) });
  }

  async function serveFile(c: C, ctx: Ctx): Promise<Response> {
    const id = c.req.param('id')!;
    const fileId = c.req.param('file') ?? '';
    const link = await store.get(id);
    if (!FILE_ID.test(fileId) || !live(link) || !ticketHolds(id, fileId, c.req.query('ticket'))) throw notFound();
    const text = await store.getFile(id, fileId);
    if (text === null) throw notFound();
    ctx.link = id;
    metrics.count('shl_files_served_total');
    emit('file.served', { id, fileId });
    return reply(200, text, 'application/jose');
  }

  let inFlight = 0;
  let draining = false;
  let lastSweep: SweepCounts | null = null;

  const remoteOf = options.remoteAddress ?? ((c: Context) => getConnInfo(c).remote.address);
  const addressOf = (c: C) => {
    let peer: string | undefined;
    try {
      peer = remoteOf(c);
    } catch {
      peer = undefined;
    }
    return clientAddress(peer, c.req.header('x-forwarded-for'), trustProxy);
  };

  const securityHeaders: Record<string, string> = {
    'cache-control': 'no-store',
    'x-content-type-options': 'nosniff',
    'referrer-policy': 'no-referrer',
    'content-security-policy': "default-src 'none'; frame-ancestors 'none'",
    ...(https ? { 'strict-transport-security': 'max-age=31536000' } : {}),
  };
  function corsFor(kind: Kind, origin: string | undefined): Record<string, string> {
    const allowed = { public: corsOrigins, management: managementCorsOrigins, ops: [] as string[] }[kind];
    const matched = allowed.includes('*') ? '*' : allowed.find((entry) => entry === origin);
    if (!matched) return {};
    return {
      'access-control-allow-origin': matched,
      'access-control-allow-headers': 'authorization, content-type',
      'access-control-allow-methods': 'GET, POST, DELETE, OPTIONS',
      'access-control-max-age': '600',
      ...(matched === '*' ? {} : { vary: 'Origin' }),
    };
  }

  function failed(ctx: Ctx, error: unknown): Response {
    if (error instanceof Refused) return reply(error.status, error.body, 'application/json', error.headers);
    if (error instanceof StoreBusy) return reply(503, { error: 'busy: try again' }, 'application/json', { 'retry-after': '1' });
    log('error', 'a request failed', { requestId: ctx.requestId, route: ctx.route, error: failure(error) });
    return reply(500, { error: 'something went wrong' });
  }

  const app = new Hono<Env>();

  // Around every request: who is asking, how many are being answered, what the answer says about
  // itself, and the line that says what happened.
  app.use('*', async (c, next) => {
    const started = Date.now();
    const ctx: Ctx = { requestId: randomBytes(6).toString('hex'), ip: '', route: 'unknown', kind: 'public' };
    c.set('ctx', ctx);
    inFlight += 1;
    let response: Response;
    try {
      ctx.ip = addressOf(c);
      await next();
      response = c.res;
    } catch (error) {
      response = failed(ctx, error);
    } finally {
      inFlight -= 1;
    }
    const headers = new Headers(response.headers);
    for (const [name, value] of Object.entries({ ...securityHeaders, ...corsFor(ctx.kind, c.req.header('origin')), 'x-request-id': ctx.requestId })) {
      headers.set(name, value);
    }
    c.res = new Response(response.body, { status: response.status, headers });
    metrics.count('shl_requests_total', { route: ctx.route, status: response.status });
    log('info', 'request', {
      requestId: ctx.requestId,
      method: c.req.method,
      route: ctx.route,
      status: response.status,
      ms: Date.now() - started,
      ip: ctx.ip,
      ...(ctx.client ? { client: ctx.client } : {}),
      ...(ctx.link ? { id: ctx.link } : {}),
    });
  });
  app.onError((error, c) => failed(c.get('ctx'), error));

  // What a request has to pass before it is looked at, by kind.
  const gate =
    (route: string, kind: Kind): MiddlewareHandler<Env> =>
    async (c, next) => {
      const ctx = c.get('ctx');
      ctx.route = route;
      ctx.kind = kind;
      if (kind !== 'ops') {
        if (draining) throw new Refused(503, { error: 'this host is shutting down' }, { 'retry-after': '1', connection: 'close' });
        if (inFlight > maxConcurrent) {
          metrics.count('shl_rate_limited_total', { bucket: 'concurrency' });
          throw new Refused(503, { error: 'busy: try again' }, { 'retry-after': '1' });
        }
      }
      if (kind === 'management' && managementAllows && !managementAllows(ctx.ip)) {
        metrics.count('shl_refused_total', { reason: 'address' });
        throw new Refused(403, { error: 'not allowed from this address' });
      }
      if (kind === 'public') limited('public', limiters.public.hit(ctx.ip));
      if (kind === 'management') limited('management', limiters.management.hit(ctx.ip));
      await next();
    };

  const handlers: Record<(typeof ROUTES)[number]['route'], (c: C, ctx: Ctx) => Promise<Response> | Response> = {
    healthz: () => reply(200, { status: 'ok' }),
    readyz: async () => {
      if (draining) return reply(503, { status: 'draining' });
      try {
        await store.ping();
      } catch (error) {
        log('error', 'the store cannot take a write', { error: failure(error) });
        return reply(503, { status: 'unavailable' });
      }
      return reply(200, { status: 'ready' });
    },
    metrics: (c, ctx) => {
      if (!metricsToken) throw notFound();
      limited('auth-failures', limiters.authFailures.blocked(ctx.ip));
      if (!same(bearer(c), metricsToken)) {
        limiters.authFailures.hit(ctx.ip);
        throw new Refused(401, { error: 'a metrics token is required' });
      }
      metrics.set('shl_in_flight', inFlight);
      if (lastSweep) {
        metrics.set('shl_links_live', lastSweep.links);
        metrics.set('shl_stored_bytes', lastSweep.bytes);
      }
      return reply(200, metrics.render(), 'text/plain; version=0.0.4');
    },
    create: makeLink,
    manifest,
    direct,
    'switch-off': switchOffByOwner,
    upload: putFile,
    access: accessList,
    file: serveFile,
  };
  for (const { method, path, route, kind } of ROUTES) {
    app.on(method, path, gate(route, kind), async (c) => handlers[route](c, c.get('ctx')));
  }

  // A browser asks whether it may, by the kind of request it means to make.
  app.options('*', (c) => {
    const ctx = c.get('ctx');
    ctx.route = 'preflight';
    ctx.kind = kindAsked((c.req.header('access-control-request-method') ?? 'GET').toUpperCase(), c.req.path);
    return reply(204);
  });
  app.notFound(async (c) => {
    // A guess at an address counts against the caller like any other request of a receiver's.
    limited('public', limiters.public.hit(c.get('ctx').ip));
    return reply(404, { error: 'not found' });
  });

  async function sweep(): Promise<SweepCounts | null> {
    const counts = await store.sweep({ now: seconds(), retainSeconds });
    if (counts) {
      lastSweep = counts;
      metrics.count('shl_sweeps_total');
      metrics.count('shl_links_emptied_total', {}, counts.emptied);
      metrics.count('shl_links_forgotten_total', {}, counts.forgotten);
    }
    return counts;
  }
  const timers: NodeJS.Timeout[] = [];
  if (sweepIntervalSeconds > 0) {
    const sweepAndSay = () => void sweep().catch((error) => log('error', 'a sweep failed', { error: failure(error) }));
    timers.push(setInterval(sweepAndSay, sweepIntervalSeconds * 1000), setTimeout(sweepAndSay, 1000));
    for (const timer of timers) timer.unref();
  }

  return {
    app,
    store,
    sweep,
    drain: () => {
      draining = true;
    },
    close: () => {
      for (const timer of timers) {
        clearInterval(timer);
        clearTimeout(timer);
      }
    },
  };
}

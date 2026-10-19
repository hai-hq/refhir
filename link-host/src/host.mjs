// A SMART Health Links host: it stores encrypted files and answers whoever opens a link.
//
// It follows the manifest protocol of the SMART Health Links specification. It never has a key: a
// file arrives encrypted, is stored as it came, and is handed out as it is, so a host that is
// broken into holds nothing that can be read. It needs Node 20 or later and no packages.
//
// `createHost(options)` returns a Node `http.Server`; `server.mjs` is the command that starts one
// from the environment. See the README for the requests it answers and how to run it.
//
// Everything that reads and then changes one link takes its turn in this process, so a link is
// served by one process: do not run two against the same data directory.
import { createHash, createHmac, randomBytes, scrypt, timingSafeEqual } from 'node:crypto';
import { mkdir, readFile, rename, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import path from 'node:path';
import { promisify } from 'node:util';

const scryptAsync = promisify(scrypt);
const sha256 = (text) => createHash('sha256').update(text).digest();
const base64url = (bytes) => Buffer.from(bytes).toString('base64url');
// Two secrets are compared by their digests, which are the same length, in constant time.
const same = (a, b) => timingSafeEqual(sha256(a), sha256(b));

const ID = /^[A-Za-z0-9_-]{43}$/; // 256 bits, which is what keeps a manifest address unguessable
const FILE_ID = /^f\d{1,6}$/;
const CONTENT_TYPE = /^[\w.+-]+\/[\w.+-]+(;[\w.+=\- ]+)*$/;
const TICKET_SECONDS = 60;
const ACCESS_KEPT = 100;
const JSON_LIMIT = 4096;

class Refused extends Error {
  constructor(status, body) {
    super(`${status}`);
    this.status = status;
    this.body = body;
  }
}

// A compact JWE that is direct and A256GCM, as the specification asks of a file. The host cannot
// open it; this only keeps a file that was never encrypted from being stored by mistake.
function looksEncrypted(text) {
  const parts = text.split('.');
  if (parts.length !== 5 || parts[1] !== '') return false;
  if (parts.some((part, index) => index !== 1 && !/^[A-Za-z0-9_-]+$/.test(part))) return false;
  try {
    const header = JSON.parse(Buffer.from(parts[0], 'base64url').toString('utf8'));
    return header.alg === 'dir' && header.enc === 'A256GCM';
  } catch {
    return false;
  }
}

export function createHost({
  adminToken,
  dataDir,
  publicUrl,
  ticketSecret = base64url(randomBytes(32)),
  maxFileBytes = 10 * 1024 * 1024,
  now = () => Date.now(),
}) {
  const seconds = () => Math.floor(now() / 1000);
  const linkDir = (id) => path.join(dataDir, id);
  const base = (request) =>
    publicUrl ? publicUrl.replace(/\/+$/, '') : `http://${request.headers.host}`;

  async function load(id) {
    try {
      return JSON.parse(await readFile(path.join(linkDir(id), 'link.json'), 'utf8'));
    } catch {
      return null;
    }
  }
  async function save(link) {
    const target = path.join(linkDir(link.id), 'link.json');
    await writeFile(`${target}.tmp`, JSON.stringify(link));
    await rename(`${target}.tmp`, target);
  }
  const isLive = (link) =>
    Boolean(link) && link.active && (!link.expiresAt || link.expiresAt > seconds());

  // Everything that reads and then changes one link takes its turn, so two requests cannot both
  // read the same count of attempts left.
  const turns = new Map();
  function exclusive(id, work) {
    const run = (turns.get(id) ?? Promise.resolve()).then(work);
    const tail = run.catch(() => {});
    turns.set(id, tail);
    tail.then(() => {
      if (turns.get(id) === tail) turns.delete(id);
    });
    return run;
  }

  const bearer = (request) => (request.headers.authorization ?? '').replace(/^Bearer /i, '');
  const manages = (link, request) =>
    Boolean(link) && timingSafeEqual(sha256(bearer(request)), Buffer.from(link.tokenHash, 'hex'));

  async function readBody(request, limit) {
    const chunks = [];
    let size = 0;
    for await (const chunk of request) {
      size += chunk.length;
      if (size > limit) throw new Refused(413, { error: 'the request is too large' });
      chunks.push(chunk);
    }
    return Buffer.concat(chunks).toString('utf8');
  }
  async function readJson(request) {
    const text = await readBody(request, JSON_LIMIT);
    try {
      const value = text ? JSON.parse(text) : {};
      if (value && typeof value === 'object' && !Array.isArray(value)) return value;
    } catch {
      // falls through to the refusal
    }
    throw new Refused(400, { error: 'the body has to be a JSON object' });
  }

  function ticketFor(id, fileId) {
    const expires = seconds() + TICKET_SECONDS;
    const mac = createHmac('sha256', ticketSecret).update(`${id}:${fileId}:${expires}`).digest();
    return `${expires}.${base64url(mac)}`;
  }
  function ticketHolds(id, fileId, ticket) {
    const [expires, mac] = (ticket ?? '').split('.');
    if (!/^\d+$/.test(expires ?? '') || !mac || Number(expires) < seconds()) return false;
    const wanted = createHmac('sha256', ticketSecret).update(`${id}:${fileId}:${expires}`).digest();
    const given = Buffer.from(mac, 'base64url');
    return given.length === wanted.length && timingSafeEqual(given, wanted);
  }

  async function makeLink(request) {
    if (!same(bearer(request), adminToken)) throw new Refused(401, { error: 'an admin token is required' });
    const options = await readJson(request);
    const attempts = options.attempts ?? 5;
    if (!Number.isInteger(attempts) || attempts < 1 || attempts > 20) {
      throw new Refused(400, { error: 'attempts is a whole number from 1 to 20' });
    }
    if (options.passcode !== undefined && (typeof options.passcode !== 'string' || !options.passcode || options.passcode.length > 64)) {
      throw new Refused(400, { error: 'passcode is a string of 1 to 64 characters' });
    }
    if (options.expiresAt !== undefined && (!Number.isInteger(options.expiresAt) || options.expiresAt <= seconds())) {
      throw new Refused(400, { error: 'expiresAt is a time in the future, in seconds since 1970' });
    }
    const id = base64url(randomBytes(32));
    const manageToken = base64url(randomBytes(32));
    const link = {
      id,
      tokenHash: sha256(manageToken).toString('hex'),
      active: true,
      expiresAt: options.expiresAt ?? null,
      attemptsLeft: attempts,
      passcode: null,
      files: [],
      access: [],
    };
    if (options.passcode) {
      const salt = randomBytes(16);
      link.passcode = {
        salt: salt.toString('hex'),
        hash: (await scryptAsync(options.passcode, salt, 32)).toString('hex'),
      };
    }
    await mkdir(linkDir(id), { recursive: true });
    await save(link);
    return [201, { id, manifestUrl: `${base(request)}/shl/${id}`, manageToken, passcodeRequired: Boolean(link.passcode) }];
  }

  async function putFile(request, id, query) {
    return exclusive(id, async () => {
      const link = await load(id);
      if (!isLive(link) || !manages(link, request)) {
        throw new Refused(link && isLive(link) ? 401 : 404, { error: 'not found, or not yours' });
      }
      const contentType = query.get('contentType') ?? 'application/fhir+json';
      if (!CONTENT_TYPE.test(contentType)) throw new Refused(400, { error: 'contentType is not a media type' });
      const text = (await readBody(request, maxFileBytes)).trim();
      if (!looksEncrypted(text)) {
        throw new Refused(400, {
          error: 'a file has to be a compact JWE (alg dir, enc A256GCM): this host stores only encrypted files',
        });
      }
      const fileId = `f${link.files.length}`;
      await writeFile(path.join(linkDir(id), `${fileId}.jwe`), text);
      link.files.push({ id: fileId, contentType, bytes: text.length });
      await save(link);
      return [201, { fileId }];
    });
  }

  async function manifest(request, id) {
    const asked = await readJson(request);
    if (typeof asked.recipient !== 'string' || !asked.recipient || asked.recipient.length > 200) {
      throw new Refused(400, { error: 'recipient is required: who is asking' });
    }
    return exclusive(id, async () => {
      const link = await load(id);
      if (!isLive(link)) throw new Refused(404, { error: 'not found' });
      if (link.passcode) {
        // A request with no passcode is not a guess, so it does not use an attempt up.
        if (typeof asked.passcode !== 'string' || !asked.passcode) {
          throw new Refused(401, { remainingAttempts: link.attemptsLeft });
        }
        const given = await scryptAsync(asked.passcode, Buffer.from(link.passcode.salt, 'hex'), 32);
        if (!timingSafeEqual(given, Buffer.from(link.passcode.hash, 'hex'))) {
          link.attemptsLeft -= 1;
          if (link.attemptsLeft <= 0) await switchOff(link);
          await save(link);
          throw new Refused(401, { remainingAttempts: Math.max(link.attemptsLeft, 0) });
        }
      }
      link.access = [...link.access, { recipient: asked.recipient, at: seconds() }].slice(-ACCESS_KEPT);
      await save(link);
      const files = [];
      for (const file of link.files) {
        const entry = { contentType: file.contentType };
        if (Number.isInteger(asked.embeddedLengthMax) && file.bytes <= asked.embeddedLengthMax) {
          entry.embedded = await readFile(path.join(linkDir(id), `${file.id}.jwe`), 'utf8');
        } else {
          entry.location = `${base(request)}/shl/${id}/file/${file.id}?ticket=${ticketFor(id, file.id)}`;
        }
        files.push(entry);
      }
      return [200, { files }];
    });
  }

  async function readStored(id, fileId) {
    return readFile(path.join(linkDir(id), `${fileId}.jwe`), 'utf8').catch(() => {
      throw new Refused(404, { error: 'not found' });
    });
  }

  async function switchOff(link) {
    link.active = false;
    link.files = [];
    await rm(linkDir(link.id), { recursive: true, force: true });
    await mkdir(linkDir(link.id), { recursive: true });
  }

  async function route(request, send) {
    const url = new URL(request.url, 'http://host');
    const parts = url.pathname.split('/').filter(Boolean);
    const method = request.method;
    if (method === 'GET' && url.pathname === '/healthz') return send(200, { status: 'ok' });
    if (parts[0] !== 'shl') throw new Refused(404, { error: 'not found' });
    if (parts.length === 1 && method === 'POST') return send(...(await makeLink(request)));
    const id = parts[1];
    if (!ID.test(id ?? '')) throw new Refused(404, { error: 'not found' });

    if (parts.length === 2 && method === 'POST') return send(...(await manifest(request, id)));
    if (parts.length === 2 && method === 'GET') {
      // The U flag: the address is the file. It cannot have a passcode, and has to say who asks.
      const recipient = url.searchParams.get('recipient');
      if (!recipient) throw new Refused(400, { error: 'recipient is required: who is asking' });
      return exclusive(id, async () => {
        const link = await load(id);
        if (!isLive(link) || link.passcode || link.files.length !== 1) throw new Refused(404, { error: 'not found' });
        link.access = [...link.access, { recipient, at: seconds() }].slice(-ACCESS_KEPT);
        await save(link);
        return send(200, await readStored(id, link.files[0].id), 'application/jose');
      });
    }
    if (parts.length === 2 && method === 'DELETE') {
      return exclusive(id, async () => {
        const link = await load(id);
        if (!link || !manages(link, request)) throw new Refused(link ? 401 : 404, { error: 'not found, or not yours' });
        await switchOff(link);
        await save(link);
        return send(204);
      });
    }
    if (parts.length === 3 && parts[2] === 'file' && method === 'POST') {
      return send(...(await putFile(request, id, url.searchParams)));
    }
    if (parts.length === 3 && parts[2] === 'access' && method === 'GET') {
      const link = await load(id);
      if (!link || !manages(link, request)) throw new Refused(link ? 401 : 404, { error: 'not found, or not yours' });
      return send(200, { access: link.access });
    }
    if (parts.length === 4 && parts[2] === 'file' && method === 'GET') {
      const fileId = parts[3];
      const link = await load(id);
      if (!FILE_ID.test(fileId) || !isLive(link) || !ticketHolds(id, fileId, url.searchParams.get('ticket'))) {
        throw new Refused(404, { error: 'not found' });
      }
      return send(200, await readStored(id, fileId), 'application/jose');
    }
    throw new Refused(404, { error: 'not found' });
  }

  return createServer(async (request, response) => {
    const send = (status, body, type = 'application/json') => {
      response.writeHead(status, {
        'content-type': type,
        'cache-control': 'no-store',
        'access-control-allow-origin': '*',
        'access-control-allow-headers': 'authorization, content-type',
        'access-control-allow-methods': 'GET, POST, DELETE, OPTIONS',
      });
      response.end(body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body));
    };
    try {
      if (request.method === 'OPTIONS') return send(204);
      await route(request, send);
    } catch (error) {
      if (error instanceof Refused) return send(error.status, error.body);
      console.error(error);
      send(500, { error: 'something went wrong' });
    }
  });
}

import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { createDecipheriv } from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';

import { createHost } from '../src/host.mjs';
import { encryptFile } from './support.mjs';

const run = promisify(execFile);
const serverPath = fileURLToPath(new URL('../src/server.mjs', import.meta.url));

const ADMIN = 'an-admin-secret-of-length';
const SUMMARY = JSON.stringify({ resourceType: 'Bundle', type: 'document', entry: [{ note: 'private-health-detail' }] });

// A host on a port of its own, with a clock the test can move.
async function startHost(options = {}) {
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'link-host-'));
  let clock = Date.now();
  const server = createHost({ adminToken: ADMIN, dataDir, now: () => clock, ...options });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return {
    base: `http://127.0.0.1:${server.address().port}`,
    dataDir,
    advance: (seconds) => {
      clock += seconds * 1000;
    },
    stop: async () => {
      await new Promise((resolve) => server.close(resolve));
      fs.rmSync(dataDir, { recursive: true, force: true });
    },
  };
}

async function call(url, { method = 'GET', token, body, headers = {} } = {}) {
  const response = await fetch(url, {
    method,
    headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), ...headers },
    body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body),
  });
  const text = await response.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    json = undefined;
  }
  return { status: response.status, text, json, headers: response.headers };
}

const makeLink = (host, options = {}) => call(`${host.base}/shl`, { method: 'POST', token: ADMIN, body: options });
const manifest = (url, body) => call(url, { method: 'POST', body });

async function linkWithFile(host, options = {}, text = SUMMARY) {
  const key = crypto.getRandomValues(new Uint8Array(32));
  const made = (await makeLink(host, options)).json;
  const put = await call(`${host.base}/shl/${made.id}/file?contentType=${encodeURIComponent('application/fhir+json;fhirVersion=4.0.1')}`, {
    method: 'POST',
    token: made.manageToken,
    body: await encryptFile(text, key),
    headers: { 'content-type': 'application/jose' },
  });
  assert.equal(put.status, 201, put.text);
  return { ...made, key };
}

function open(jwe, key) {
  const [header, , iv, ciphertext, tag] = jwe.split('.');
  const decipher = createDecipheriv('aes-256-gcm', key, Buffer.from(iv, 'base64url'));
  decipher.setAAD(Buffer.from(header));
  decipher.setAuthTag(Buffer.from(tag, 'base64url'));
  return Buffer.concat([decipher.update(Buffer.from(ciphertext, 'base64url')), decipher.final()]).toString('utf8');
}

test('only the admin makes links, and only the link\'s own token puts a file on it', async () => {
  const host = await startHost();
  try {
    assert.equal((await call(`${host.base}/shl`, { method: 'POST', body: {} })).status, 401);
    assert.equal((await call(`${host.base}/shl`, { method: 'POST', token: 'wrong', body: {} })).status, 401);
    const first = (await makeLink(host)).json;
    const second = (await makeLink(host)).json;
    assert.notEqual(first.id, second.id);
    assert.equal(first.id.length, 43, '256 bits of address');
    const file = await encryptFile(SUMMARY, crypto.getRandomValues(new Uint8Array(32)));
    const put = (id, token) => call(`${host.base}/shl/${id}/file`, { method: 'POST', token, body: file });
    assert.equal((await put(first.id, undefined)).status, 401);
    assert.equal((await put(first.id, second.manageToken)).status, 401, 'another link\'s token does not do');
    assert.equal((await put(first.id, ADMIN)).status, 401, 'nor does the admin token');
    assert.equal((await put(first.id, first.manageToken)).status, 201);
  } finally {
    await host.stop();
  }
});

test('a file that is not encrypted, or is too large, is refused and not stored', async () => {
  const host = await startHost({ maxFileBytes: 2000 });
  try {
    const made = (await makeLink(host)).json;
    const put = (body, query = '') => call(`${host.base}/shl/${made.id}/file${query}`, { method: 'POST', token: made.manageToken, body });
    const plain = await put(SUMMARY);
    assert.equal(plain.status, 400);
    assert.match(plain.json.error, /only encrypted files/);
    assert.equal((await put('a.b.c.d.e')).status, 400);
    const header = Buffer.from(JSON.stringify({ alg: 'RSA-OAEP', enc: 'A256GCM' })).toString('base64url');
    assert.equal((await put(`${header}.x.y.z.w`)).status, 400, 'a key wrapped for someone else is not what a link holds');
    const real = await encryptFile('x', crypto.getRandomValues(new Uint8Array(32)));
    assert.equal((await put(real, '?contentType=not%20a%20type')).status, 400);
    assert.equal((await put('x'.repeat(5000))).status, 413);
    const manifestNow = await manifest(`${host.base}/shl/${made.id}`, { recipient: 'r' });
    assert.deepEqual(manifestNow.json.files, [], 'nothing was stored');
  } finally {
    await host.stop();
  }
});

test('a passcode is counted, an absent one is not a guess, and running out switches the link off', async () => {
  const host = await startHost();
  try {
    const link = await linkWithFile(host, { passcode: 'open sesame', attempts: 3 });
    const ask = (passcode) => manifest(`${host.base}/shl/${link.id}`, { recipient: 'r', ...(passcode ? { passcode } : {}) });
    const none = await ask();
    assert.deepEqual([none.status, none.json], [401, { remainingAttempts: 3 }]);
    const wrongOnce = await ask('nope');
    assert.deepEqual([wrongOnce.status, wrongOnce.json], [401, { remainingAttempts: 2 }]);
    assert.equal((await ask('open sesame')).status, 200, 'the right passcode still works');
    assert.deepEqual((await ask('nope')).json, { remainingAttempts: 1 });
    assert.deepEqual((await ask('nope')).json, { remainingAttempts: 0 });
    assert.equal((await ask('open sesame')).status, 404, 'and now not even the right one');
    const file = path.join(host.dataDir, link.id);
    assert.deepEqual(fs.readdirSync(file), ['link.json'], 'the files went with the link');
    const stored = fs.readFileSync(path.join(file, 'link.json'), 'utf8');
    assert.ok(!stored.includes('open sesame'), 'the passcode is stored hashed');
  } finally {
    await host.stop();
  }
});

test('guesses that arrive together are each counted', async () => {
  const host = await startHost();
  try {
    const link = await linkWithFile(host, { passcode: 'right', attempts: 5 });
    const replies = await Promise.all(
      Array.from({ length: 12 }, () => manifest(`${host.base}/shl/${link.id}`, { recipient: 'r', passcode: 'wrong' })),
    );
    const refused = replies.filter((reply) => reply.status === 401);
    assert.equal(refused.length, 5, 'five guesses, and not one more');
    assert.deepEqual(refused.map((reply) => reply.json.remainingAttempts).sort(), [0, 1, 2, 3, 4]);
    assert.equal(replies.filter((reply) => reply.status === 404).length, 7);
  } finally {
    await host.stop();
  }
});

test('a file is handed out only to someone holding the ticket the manifest gave, for a minute', async () => {
  const host = await startHost();
  try {
    const link = await linkWithFile(host);
    const files = (await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' })).json.files;
    const location = files[0].location;
    assert.equal((await call(location)).status, 200);
    assert.equal((await call(location.split('?')[0])).status, 404, 'no ticket');
    assert.equal((await call(`${location.split('?')[0]}?ticket=1.AAAA`)).status, 404, 'a made-up ticket');
    const [expires, mac] = new URL(location).searchParams.get('ticket').split('.');
    assert.equal((await call(`${location.split('?')[0]}?ticket=${Number(expires) + 1000}.${mac}`)).status, 404, 'a ticket with a later expiry');
    const other = await linkWithFile(host);
    const crossed = location.replace(link.id, other.id);
    assert.equal((await call(crossed)).status, 404, 'a ticket for another link');
    host.advance(59);
    assert.equal((await call(location)).status, 200);
    host.advance(5);
    assert.equal((await call(location)).status, 404, 'a minute later it is spent');
    const again = (await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' })).json.files[0].location;
    assert.equal((await call(again)).status, 200, 'and asking again gives a new one');
  } finally {
    await host.stop();
  }
});

test('a link that expires, or is switched off, answers as if it were not there', async () => {
  const host = await startHost();
  try {
    const soon = await linkWithFile(host, { expiresAt: Math.floor(Date.now() / 1000) + 100 });
    assert.equal((await manifest(`${host.base}/shl/${soon.id}`, { recipient: 'r' })).status, 200);
    host.advance(200);
    assert.equal((await manifest(`${host.base}/shl/${soon.id}`, { recipient: 'r' })).status, 404);

    const link = await linkWithFile(host);
    const location = (await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' })).json.files[0].location;
    assert.equal((await call(`${host.base}/shl/${link.id}`, { method: 'DELETE' })).status, 401);
    assert.equal((await call(`${host.base}/shl/${link.id}`, { method: 'DELETE', token: link.manageToken })).status, 204);
    assert.equal((await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' })).status, 404);
    assert.equal((await call(location)).status, 404, 'a ticket already given does not outlive the link');
    assert.deepEqual(fs.readdirSync(path.join(host.dataDir, link.id)), ['link.json']);
    assert.equal((await call(`${host.base}/shl/${link.id}/file`, { method: 'POST', token: link.manageToken, body: 'x' })).status, 404);
    assert.equal((await makeLink(host, { expiresAt: 5 })).status, 400, 'a link cannot start out expired');
  } finally {
    await host.stop();
  }
});

test('the U flag: the address is the file, for a link with no passcode and one file', async () => {
  const host = await startHost();
  try {
    const link = await linkWithFile(host);
    assert.equal((await call(`${host.base}/shl/${link.id}`)).status, 400, 'who is asking is required');
    const direct = await call(`${host.base}/shl/${link.id}?recipient=Dr%20Okafor`);
    assert.equal(direct.status, 200);
    assert.equal(open(direct.text, link.key), SUMMARY);
    const locked = await linkWithFile(host, { passcode: 'p' });
    assert.equal((await call(`${host.base}/shl/${locked.id}?recipient=r`)).status, 404, 'a passcode link is never given away by address');
    const empty = (await makeLink(host)).json;
    assert.equal((await call(`${host.base}/shl/${empty.id}?recipient=r`)).status, 404);
  } finally {
    await host.stop();
  }
});

test('who has asked is kept, for the link\'s own token to read', async () => {
  const host = await startHost();
  try {
    const link = await linkWithFile(host);
    await manifest(`${host.base}/shl/${link.id}`, { recipient: 'Dr Okafor' });
    await call(`${host.base}/shl/${link.id}?recipient=Nurse%20Ada`);
    assert.equal((await call(`${host.base}/shl/${link.id}/access`)).status, 401);
    const log = (await call(`${host.base}/shl/${link.id}/access`, { token: link.manageToken })).json.access;
    assert.deepEqual(log.map((entry) => entry.recipient), ['Dr Okafor', 'Nurse Ada']);
    assert.equal((await manifest(`${host.base}/shl/${link.id}`, {})).status, 400, 'a manifest request says who it is from');
  } finally {
    await host.stop();
  }
});

test('a file small enough is sent inside the manifest when the receiver asks for that', async () => {
  const host = await startHost();
  try {
    const link = await linkWithFile(host);
    const ask = (embeddedLengthMax) => manifest(`${host.base}/shl/${link.id}`, { recipient: 'r', embeddedLengthMax });
    const inline = (await ask(1_000_000)).json.files[0];
    assert.ok(inline.embedded && !inline.location);
    assert.equal(open(inline.embedded, link.key), SUMMARY);
    const apart = (await ask(10)).json.files[0];
    assert.ok(apart.location && !apart.embedded);
  } finally {
    await host.stop();
  }
});

test('a path that is not a link address reaches nothing, and any origin may call it', async () => {
  const host = await startHost();
  try {
    for (const bad of ['/shl/..%2F..%2Fetc%2Fpasswd', '/shl/short', '/shl/../x', '/other', `/shl/${'a'.repeat(43)}`]) {
      const reply = await call(`${host.base}${bad}`, { method: 'POST', body: { recipient: 'r' } });
      assert.equal(reply.status, 404, bad);
    }
    const preflight = await call(`${host.base}/shl`, { method: 'OPTIONS' });
    assert.equal(preflight.status, 204);
    assert.equal(preflight.headers.get('access-control-allow-origin'), '*');
    assert.match(preflight.headers.get('access-control-allow-headers'), /authorization/);
    const made = await makeLink(host);
    assert.equal(made.headers.get('access-control-allow-origin'), '*');
    assert.equal(made.headers.get('cache-control'), 'no-store');
    assert.equal((await call(`${host.base}/healthz`)).status, 200);
  } finally {
    await host.stop();
  }
});

test('the host will not start without an admin token it can trust', async () => {
  await assert.rejects(
    run('node', [serverPath], { env: { PATH: process.env.PATH } }),
    (error) => error.code === 1 && /Set ADMIN_TOKEN to a secret of 16 characters or more/.test(error.stderr),
  );
  await assert.rejects(
    run('node', [serverPath], { env: { PATH: process.env.PATH, ADMIN_TOKEN: 'short' } }),
    (error) => error.code === 1,
  );
});

test('the command starts, answers, and goes when it is told to stop', async () => {
  const free = await new Promise((resolve) => {
    const probe = net.createServer().listen(0, '127.0.0.1', () => {
      const { port } = probe.address();
      probe.close(() => resolve(port));
    });
  });
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'link-host-cli-'));
  const child = spawn('node', [serverPath], {
    env: { PATH: process.env.PATH, ADMIN_TOKEN: ADMIN, PORT: String(free), DATA_DIR: dataDir, PUBLIC_URL: 'https://links.example.org/' },
  });
  try {
    let answered;
    for (let attempt = 0; attempt < 50 && !answered; attempt += 1) {
      answered = await fetch(`http://127.0.0.1:${free}/healthz`).catch(() => undefined);
      if (!answered) await new Promise((resolve) => setTimeout(resolve, 100));
    }
    assert.equal(answered?.status, 200);
    const made = await call(`http://127.0.0.1:${free}/shl`, { method: 'POST', token: ADMIN, body: {} });
    assert.equal(made.status, 201);
    assert.match(made.json.manifestUrl, /^https:\/\/links\.example\.org\/shl\/[A-Za-z0-9_-]{43}$/, 'PUBLIC_URL, without its trailing slash');
    const exited = new Promise((resolve) => child.once('exit', (code, signal) => resolve({ code, signal })));
    child.kill('SIGTERM');
    assert.deepEqual(await exited, { code: 0, signal: null });
  } finally {
    child.kill('SIGKILL');
    fs.rmSync(dataDir, { recursive: true, force: true });
  }
});

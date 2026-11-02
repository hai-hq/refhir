import { createHash } from 'node:crypto';

import { describe, expect, it } from 'vitest';

import type { HostEvent } from '../src/types.js';
import { ADMIN, call, linkWithFile, makeLink, manifest, putFile, startHost, storeForTests, using, type Running } from './support.js';

const DAY = 86_400;
const seconds = (host: { now(): number }) => Math.floor(host.now() / 1000);

const admin = (host: { base: string }, path = '', { method = 'GET', body, token = ADMIN }: { method?: string; body?: unknown; token?: string } = {}) =>
  call(`${host.base}/admin/clients${path}`, { method, token, ...(body === undefined ? {} : { body }) });
async function newClient(host: { base: string }, body: Record<string, unknown>) {
  const made = await admin(host, '', { method: 'POST', body });
  expect(made.status, made.text).toBe(201);
  return made.json as { id: string; name: string; token: string };
}
const linksOf = (host: { base: string }, token: string, query = '') => call(`${host.base}/shl${query}`, { token });
const accessOf = (host: { base: string }, id: string, token: string) => call(`${host.base}/shl/${id}/access`, { token });
const switchOff = (host: { base: string }, id: string, token: string) => call(`${host.base}/shl/${id}`, { method: 'DELETE', token });
const opens = async (host: { base: string }, id: string) => (await manifest(`${host.base}/shl/${id}`, { recipient: 'r' })).status;

describe('a client made over the admin API', () => {
  it('can make links at once, in every copy of the host, and its token is said once and never kept', async () => {
    let now = Date.now();
    const { store, done } = await storeForTests(() => now);
    const events: HostEvent[] = [];
    // Two copies of the host on one database, both started before the client was made.
    const first = await startHost({ store, onEvent: (event) => void events.push(event) });
    const second = await startHost({ store });
    try {
      const made = await newClient(first, { name: 'project-a', maxFilesPerLink: 1, defaultLifetimeDays: 2, maxLifetimeDays: 7 });
      expect(made.id).toMatch(/^cl_[A-Za-z0-9_-]{22}$/);
      expect(made.token).toMatch(/^[A-Za-z0-9_-]{43}$/);
      expect(made).toMatchObject({ name: 'project-a', source: 'api', maxFilesPerLink: 1, defaultLifetimeDays: 2, maxLifetimeDays: 7 });
      expect(made).not.toHaveProperty('tokenHash');

      const link = await makeLink(second, {}, made.token);
      expect(link.status).toBe(201);
      expect(link.json.expiresAt).toBe(seconds(second) + 2 * DAY); // the client's own default
      expect((await makeLink(second, { expiresAt: seconds(second) + 8 * DAY }, made.token)).status).toBe(400); // and its own longest
      expect((await putFile(second, link.json)).status).toBe(201);
      expect((await putFile(second, link.json)).status).toBe(409); // one file is all its links may hold
      const kept = await store.get(link.json.id);
      expect([kept?.createdBy, kept?.clientId]).toEqual(['project-a', made.id]);

      const listed = await admin(first);
      expect(listed.status).toBe(200);
      expect(listed.json.clients.map((client: { name: string; source: string }) => [client.name, client.source])).toEqual([
        ['admin', 'config'],
        ['project-a', 'api'],
      ]);
      expect((await admin(second, `/${made.id}`)).json).toMatchObject({ id: made.id, name: 'project-a', maxFilesPerLink: 1 });
      // Neither the token nor its hash is said again.
      const hash = createHash('sha256').update(made.token).digest('hex');
      for (const reply of [listed, await admin(first, `/${made.id}`)]) {
        expect(reply.text.includes(made.token) || reply.text.includes(hash)).toBe(false);
      }
      expect(events.find((event) => event.type === 'client.created')).toMatchObject({ clientId: made.id, client: 'project-a' });
    } finally {
      await first.stop();
      await second.stop();
      await done();
    }
  });

  it('is refused once it is revoked, and its links go on unless they are ended with it', () =>
    using(async (host) => {
      const a = await newClient(host, { name: 'a' });
      const b = await newClient(host, { name: 'b' });
      const aLink = (await makeLink(host, {}, a.token)).json;
      await putFile(host, aLink);
      const bLink = (await makeLink(host, {}, b.token)).json;
      await putFile(host, bLink);

      expect((await admin(host, `/${a.id}`, { method: 'DELETE' })).status).toBe(204);
      expect((await makeLink(host, {}, a.token)).status).toBe(401);
      expect((await linksOf(host, a.token)).status).toBe(401);
      expect((await accessOf(host, aLink.id, a.token)).status).toBe(401);
      expect(await opens(host, aLink.id)).toBe(200); // the link it made still opens
      expect((await accessOf(host, aLink.id, aLink.manageToken)).status).toBe(200); // and its own token still reads it
      expect((await admin(host, `/${a.id}`)).status).toBe(404);
      expect((await admin(host, `/${a.id}`, { method: 'DELETE' })).status).toBe(404);

      const ended = await admin(host, `/${b.id}?endLinks=true`, { method: 'DELETE' });
      expect([ended.status, ended.json]).toEqual([200, { endedLinks: 1, more: false }]);
      expect(await opens(host, bLink.id)).toBe(404);
      expect((await makeLink(host, {}, b.token)).status).toBe(401);
      expect((await admin(host, `/${a.id}?endLinks=maybe`, { method: 'DELETE' })).status).toBe(400);

      // A name is free again once its client is gone, and the new client is not the old one.
      const again = await newClient(host, { name: 'a' });
      expect(again.id).not.toBe(a.id);
      expect((await linksOf(host, again.token)).json.links).toEqual([]);
      expect((await accessOf(host, aLink.id, again.token)).status).toBe(401);
    }));

  it('can have its links ended after it is gone, so that a revoke cut short is made again', () =>
    using(async (host) => {
      const a = await newClient(host, { name: 'a' });
      const link = (await makeLink(host, {}, a.token)).json;
      expect((await admin(host, `/${a.id}`, { method: 'DELETE' })).status).toBe(204); // the client is gone, and its link is not
      expect(await opens(host, link.id)).toBe(200);
      const again = await admin(host, `/${a.id}?endLinks=true`, { method: 'DELETE' });
      expect([again.status, again.json]).toEqual([200, { endedLinks: 1, more: false }]);
      expect(await opens(host, link.id)).toBe(404);
      const third = await admin(host, `/${a.id}?endLinks=true`, { method: 'DELETE' });
      expect([third.status, third.json]).toEqual([200, { endedLinks: 0, more: false }]); // its links are still there to be seen, ended
      // Nothing of a client that never was.
      expect((await admin(host, '/cl_bbbbbbbbbbbbbbbbbbbbbb?endLinks=true', { method: 'DELETE' })).status).toBe(404);
    }));

  it('is refused its old token once it is rotated, and keeps its links', () =>
    using(async (host) => {
      const a = await newClient(host, { name: 'a' });
      const before = (await makeLink(host, {}, a.token)).json;
      const rotated = await admin(host, `/${a.id}/rotate`, { method: 'POST' });
      expect(rotated.status).toBe(200);
      expect(rotated.json).toMatchObject({ id: a.id, name: 'a' });
      expect(rotated.json.token).toMatch(/^[A-Za-z0-9_-]{43}$/);
      expect(rotated.json.token).not.toBe(a.token);

      expect((await makeLink(host, {}, a.token)).status).toBe(401);
      expect((await linksOf(host, a.token)).status).toBe(401);
      expect((await accessOf(host, before.id, a.token)).status).toBe(401);
      expect((await makeLink(host, {}, rotated.json.token)).status).toBe(201);
      const listed = (await linksOf(host, rotated.json.token)).json.links.map((link: { id: string }) => link.id);
      expect(listed).toHaveLength(2);
      expect(listed).toContain(before.id); // a link made with the old token is still the client's
      expect((await accessOf(host, before.id, rotated.json.token)).status).toBe(200);
    }));

  it('takes a change of its settings at once: where it may call from, its limits, the files its links hold', () =>
    using({ trustProxy: 1 }, async (host) => {
      const a = await newClient(host, { name: 'a', allow: ['203.0.113.10'], allowedContentTypes: ['application/smart-health-card'] });
      const from = (ip: string) => call(`${host.base}/shl`, { method: 'POST', token: a.token, body: {}, headers: { 'x-forwarded-for': ip } });
      const link = await from('203.0.113.10');
      expect(link.status).toBe(201);
      expect((await from('192.0.2.1')).status).toBe(403);
      expect((await putFile(host, link.json)).status).toBe(400); // application/fhir+json is not a type its links hold
      expect((await putFile(host, link.json, undefined, undefined, '?contentType=application/smart-health-card')).status).toBe(201);

      const changed = await admin(host, `/${a.id}`, { method: 'PATCH', body: { allow: null, requirePasscode: true, name: 'a-renamed' } });
      expect(changed.status, changed.text).toBe(200);
      expect(changed.json).toMatchObject({ name: 'a-renamed', requirePasscode: true, allowedContentTypes: ['application/smart-health-card'] });
      expect(changed.json).not.toHaveProperty('allow');
      expect((await from('192.0.2.1')).status).toBe(400); // let in from anywhere now, and told a link needs a passcode
      const made = await call(`${host.base}/shl`, { method: 'POST', token: a.token, body: { passcode: 'p' }, headers: { 'x-forwarded-for': '192.0.2.1' } });
      expect(made.status).toBe(201);
      expect((await host.host.store.get(made.json.id))?.createdBy).toBe('a-renamed');
    }));
});

describe('the links of a client', () => {
  it('are listed, read about and switched off by that client alone', () =>
    using({ clients: [{ name: 'ehr', token: 'ehr-token-of-sixteen-chars' }] }, async (host) => {
      const a = await newClient(host, { name: 'a' });
      const b = await newClient(host, { name: 'b' });
      const first = (await makeLink(host, { passcode: 'p' }, a.token)).json;
      await putFile(host, first);
      const second = (await makeLink(host, {}, a.token)).json;
      const theirs = (await makeLink(host, {}, b.token)).json;
      const ehr = (await makeLink(host, {}, 'ehr-token-of-sixteen-chars')).json;
      await manifest(`${host.base}/shl/${first.id}`, { recipient: 'Dr Okafor', passcode: 'p' });
      await manifest(`${host.base}/shl/${theirs.id}`, { recipient: 'Dr Adeyemi' });

      const mine = await linksOf(host, a.token);
      expect(mine.status).toBe(200);
      expect(mine.json.links.map((link: { id: string }) => link.id).sort()).toEqual([first.id, second.id].sort());
      expect(mine.json.links.find((link: { id: string }) => link.id === first.id)).toEqual({
        id: first.id,
        manifestUrl: `${host.base}/shl/${first.id}`,
        createdAt: seconds(host),
        expiresAt: first.expiresAt,
        endedAt: null,
        active: true,
        passcodeRequired: true,
        files: 1,
      });
      expect(mine.text.includes(first.manageToken)).toBe(false);
      expect((await linksOf(host, b.token)).json.links.map((link: { id: string }) => link.id)).toEqual([theirs.id]);
      expect((await linksOf(host, 'ehr-token-of-sixteen-chars')).json.links.map((link: { id: string }) => link.id)).toEqual([ehr.id]); // a client of the settings too
      expect((await linksOf(host, ADMIN)).json.links).toEqual([]); // and the admin token's own, which are none
      expect((await linksOf(host, 'not-a-token-of-anyone')).status).toBe(401);

      // Who asked: its own links, and no one else's.
      const asked = await accessOf(host, first.id, a.token);
      expect(asked.status).toBe(200);
      expect(asked.json.access.map((entry: { recipient: string }) => entry.recipient)).toEqual(['Dr Okafor']);
      const notMine = await accessOf(host, theirs.id, a.token);
      expect([notMine.status, notMine.json]).toEqual([401, { error: 'not found, or not yours' }]);
      expect(notMine.text.includes('Dr Adeyemi')).toBe(false);
      expect((await accessOf(host, ehr.id, b.token)).status).toBe(401);
      expect((await accessOf(host, first.id, ADMIN)).status).toBe(401); // the admin token manages clients, not their links
      expect((await accessOf(host, 'x'.repeat(43), a.token)).status).toBe(404);

      // Switching off: its own links, and no one else's.
      expect((await switchOff(host, theirs.id, a.token)).status).toBe(401);
      expect(await opens(host, theirs.id)).toBe(200);
      expect((await switchOff(host, second.id, a.token)).status).toBe(204);
      expect(await opens(host, second.id)).toBe(404);
      expect((await linksOf(host, a.token)).json.links.find((link: { id: string }) => link.id === second.id)).toMatchObject({
        active: false,
        endedAt: seconds(host),
      });
      // A link's own token still works as it did.
      expect((await switchOff(host, theirs.id, theirs.manageToken)).status).toBe(204);
    }));

  it('are listed a page at a time, newest first', () =>
    using(async (host: Running) => {
      const a = await newClient(host, { name: 'a' });
      const made: string[] = [];
      for (let index = 0; index < 3; index += 1) {
        made.push((await makeLink(host, {}, a.token)).json.id);
        host.advance(index === 0 ? 1 : 0); // two in the same second, which the id then orders
      }
      const page = await linksOf(host, a.token, '?limit=2');
      expect(page.json.links).toHaveLength(2);
      expect(page.json.next).toMatch(/^\d+\.[A-Za-z0-9_-]{43}$/);
      const rest = await linksOf(host, a.token, `?limit=2&before=${page.json.next}`);
      expect(rest.json.links).toHaveLength(1);
      expect(rest.json).not.toHaveProperty('next');
      const ids = [...page.json.links, ...rest.json.links].map((link: { id: string }) => link.id);
      expect(new Set(ids).size).toBe(3);
      expect(ids[2]).toBe(made[0]); // the oldest last
      expect((await linksOf(host, a.token, '?limit=0')).status).toBe(400);
      expect((await linksOf(host, a.token, '?limit=1001')).status).toBe(400);
      expect((await linksOf(host, a.token, '?before=nonsense')).status).toBe(400);
    }));
});

describe('the admin API', () => {
  it('answers the admin token alone, and is not there without one', async () => {
    await using(async (host) => {
      const a = await newClient(host, { name: 'a' });
      expect((await admin(host, '', { token: '' })).status).toBe(401);
      expect((await admin(host, '', { token: a.token })).status).toBe(401); // a client's token manages links, not clients
      expect((await admin(host, '', { method: 'POST', body: { name: 'b' }, token: a.token })).status).toBe(401);
      expect((await admin(host, `/${a.id}/rotate`, { method: 'POST', token: a.token })).status).toBe(401);
    });
    await using({ adminToken: undefined as unknown as string, clients: [{ name: 'ehr', token: 'ehr-token-of-sixteen-chars' }] }, async (host) => {
      expect((await admin(host, '', { token: 'ehr-token-of-sixteen-chars' })).status).toBe(404);
    });
  });

  it('keeps both of two changes made at once, and does not take a token that is not its own for a client of the host\'s code', async () => {
    await using(async (host) => {
      const a = await newClient(host, { name: 'a', maxAttempts: 7 });
      const patch = (body: unknown) => admin(host, `/${a.id}`, { method: 'PATCH', body });
      const replies = await Promise.all([patch({ requirePasscode: true }), patch({ maxFilesPerLink: 3 }), patch({ maxAttempts: null })]);
      expect(replies.map((reply) => reply.status)).toEqual([200, 200, 200]);
      const kept = (await admin(host, `/${a.id}`)).json;
      expect(kept).toMatchObject({ requirePasscode: true, maxFilesPerLink: 3 });
      expect(kept).not.toHaveProperty('maxAttempts');
    });
    // A host with its own way to tell who is calling still takes the clients made over the API.
    await using({ authenticate: (request) => (request.headers.get('x-api-key') === 'let-me-in' ? { name: 'sso-user' } : null) }, async (host) => {
      const a = await newClient(host, { name: 'a' });
      expect((await makeLink(host, {}, a.token)).status).toBe(201);
      expect((await call(`${host.base}/shl`, { token: a.token })).status).toBe(200);
      expect((await call(`${host.base}/shl`, { method: 'POST', body: {}, headers: { 'x-api-key': 'let-me-in' } })).status).toBe(201);
      expect((await makeLink(host, {}, 'not-the-token-of-anyone')).status).toBe(401);
    });
  });

  it('says what is wrong with a client it is asked to make or change, and leaves a client of the settings alone', () =>
    using({ clients: [{ name: 'ehr', token: 'ehr-token-of-sixteen-chars', maxFilesPerLink: 2 }] }, async (host) => {
      const post = (body: unknown) => admin(host, '', { method: 'POST', body });
      expect((await post({})).json.error).toMatch(/name is required/);
      const wrong = await post({ name: 'x', maxFilesPerLinks: 2, maxFileBytes: 'big', allow: ['300.0.0.1'] });
      expect(wrong.status).toBe(400);
      expect(wrong.json.error).toMatch(/maxFilesPerLinks is not a setting of a client/);
      expect(wrong.json.error).toMatch(/maxFileBytes is a whole number above 0/);
      expect(wrong.json.error).toMatch(/300.0.0.1 is not an address/);
      expect((await post({ name: 'x', allowedContentTypes: ['not a type'] })).status).toBe(400);
      expect((await post({ name: 'x', maxFileBytes: 5_000_000_000 })).json.error).toMatch(/maxFileBytes is a whole number above 0 and up to 2147483647/); // links keep it in 32 bits
      expect((await post({ name: 'has spaces' })).status).toBe(400);
      expect((await post({ name: 'cl_aaaaaaaaaaaaaaaaaaaaaa' })).status).toBe(400); // the shape of an id
      expect((await post({ name: 'x', token: 'my-own-token-of-sixteen' })).status).toBe(400); // a token is made by the host
      expect((await post('[1]')).status).toBe(400);

      const a = await newClient(host, { name: 'a' });
      expect((await post({ name: 'a' })).status).toBe(409);
      expect((await post({ name: 'ehr' })).status).toBe(409);
      expect((await post({ name: 'admin' })).status).toBe(409);
      await newClient(host, { name: 'b' });
      expect((await admin(host, `/${a.id}`, { method: 'PATCH', body: { name: 'b' } })).status).toBe(409);
      expect((await admin(host, `/${a.id}`, { method: 'PATCH', body: { name: 'ehr' } })).status).toBe(409);
      expect((await admin(host, `/${a.id}`, { method: 'PATCH', body: { maxAttempts: 0 } })).status).toBe(400);
      expect((await admin(host, `/${a.id}`, { method: 'PATCH', body: { name: null } })).status).toBe(400);

      // A client of the settings is listed and read, and changed only where it was set.
      expect((await admin(host, '/ehr')).json).toEqual({ id: 'ehr', name: 'ehr', source: 'config', maxFilesPerLink: 2 });
      for (const [method, path] of [['PATCH', '/ehr'], ['POST', '/ehr/rotate'], ['DELETE', '/ehr'], ['DELETE', '/admin']]) {
        const refused = await admin(host, path, { method, body: {} });
        expect([refused.status, refused.json.error], `${method} ${path}`).toEqual([409, expect.stringMatching(/set in the host's settings/)]);
      }
      expect((await admin(host, '/cl_bbbbbbbbbbbbbbbbbbbbbb')).status).toBe(404);
      expect((await admin(host, '/nobody', { method: 'PATCH', body: {} })).status).toBe(404);
    }));

  it('says to a browser that it may change a client, from the pages that may manage', () =>
    using({ managementCorsOrigins: ['https://console.example'] }, async (host) => {
      const asked = await call(`${host.base}/admin/clients/cl_aaaaaaaaaaaaaaaaaaaaaa`, {
        method: 'OPTIONS',
        headers: { origin: 'https://console.example', 'access-control-request-method': 'PATCH' },
      });
      expect(asked.headers.get('access-control-allow-origin')).toBe('https://console.example');
      expect(asked.headers.get('access-control-allow-methods')).toMatch(/PATCH/);
      const elsewhere = await call(`${host.base}/admin/clients`, {
        method: 'OPTIONS',
        headers: { origin: 'https://elsewhere.example', 'access-control-request-method': 'POST' },
      });
      expect(elsewhere.headers.get('access-control-allow-origin')).toBeNull();
    }));
});

describe('a link made by a client of the settings, before clients were kept', () => {
  it('is that client\'s, by its name', () =>
    using({ clients: [{ name: 'ehr', token: 'ehr-token-of-sixteen-chars' }] }, async (host) => {
      // What a store held before a link said whose it was: only the name of who made it.
      const link = await linkWithFile(host, {}, undefined);
      const kept = (await host.host.store.get(link.id))!;
      expect(kept.clientId).toBe('admin');
      expect((await accessOf(host, link.id, ADMIN)).status).toBe(200);
      expect((await accessOf(host, link.id, 'ehr-token-of-sixteen-chars')).status).toBe(401);
    }));

  it('cannot be taken by a client of the settings with a name of the shape of an id', async () => {
    await expect(
      startHost({ clients: [{ name: 'cl_aaaaaaaaaaaaaaaaaaaaaa', token: 'a-token-of-sixteen-chars' }] }),
    ).rejects.toThrow(/the shape of the id of a client made over the admin API/);
  });
});

import { createHash } from 'node:crypto';

import { describe, expect, it } from 'vitest';

import type { HostEvent } from '../src/types.js';
import { ADMIN, SUMMARY, call, linkWithFile, makeLink, manifest, putFile, startHost, using } from './support.js';

const DAY = 86_400;
const seconds = (host: { now(): number }) => Math.floor(host.now() / 1000);

describe('who may make links', () => {
  it('is each client, with a token of its own and the limits it was given', async () => {
    const hash = createHash('sha256').update('portal-token-of-sixteen').digest('hex');
    const events: HostEvent[] = [];
    await using(
      {
        adminToken: undefined as unknown as string,
        clients: [
          { name: 'ehr', token: 'ehr-token-of-sixteen-chars', maxLifetimeSeconds: 7 * DAY, defaultLifetimeSeconds: DAY },
          { name: 'portal', tokenHash: `sha256:${hash}`, requirePasscode: true, maxFilesPerLink: 1 },
        ],
        onEvent: (event) => void events.push(event),
      },
      async (host) => {
        const ehr = await makeLink(host, {}, 'ehr-token-of-sixteen-chars');
        expect(ehr.status).toBe(201);
        expect(ehr.json.expiresAt).toBe(seconds(host) + DAY); // the client's own default
        expect((await makeLink(host, { expiresAt: seconds(host) + 30 * DAY }, 'ehr-token-of-sixteen-chars')).status).toBe(400); // beyond what it may ask
        expect((await makeLink(host, { expiresAt: seconds(host) + 6 * DAY }, 'ehr-token-of-sixteen-chars')).status).toBe(201);

        expect((await makeLink(host, {}, 'portal-token-of-sixteen')).status).toBe(400); // the portal has to give a passcode
        const portal = await makeLink(host, { passcode: 'p' }, 'portal-token-of-sixteen');
        expect(portal.status).toBe(201);
        expect((await putFile(host, portal.json)).status).toBe(201);
        expect((await putFile(host, portal.json)).status).toBe(409); // and one file is all it may put on a link

        expect((await makeLink(host, {}, ADMIN)).status).toBe(401); // a token nobody was given
        expect((await makeLink(host, {}, 'portal-token-of-sixtee')).status).toBe(401); // nor one that is nearly right
        expect((await makeLink(host, {}, hash)).status).toBe(401); // nor the hash, which is not the token

        expect(events.filter((event) => event.type === 'link.created').map((event) => event.client)).toEqual(['ehr', 'ehr', 'portal']);
        expect((await host.host.store.get(ehr.json.id))?.createdBy).toBe('ehr'); // a link says who made it
      },
    );
  });

  it('is never given a longer life than its client may ask for, when it asks for none', () =>
    using(
      { adminToken: undefined as unknown as string, clients: [{ name: 'short', token: 'short-token-of-sixteen', maxLifetimeSeconds: 3 * DAY }] },
      async (host) => {
        // The host's usual is thirty days, which is more than this client is allowed.
        const made = await makeLink(host, {}, 'short-token-of-sixteen');
        expect(made.json.expiresAt).toBe(seconds(host) + 3 * DAY);
        expect((await makeLink(host, { expiresAt: seconds(host) + 4 * DAY }, 'short-token-of-sixteen')).status).toBe(400);
      },
    ));

  it("can be told by the host's own code", () =>
    using(
      {
        adminToken: undefined as unknown as string,
        authenticate: (request) => (request.headers.get('x-api-key') === 'let-me-in' ? { name: 'sso-user', maxFilesPerLink: 1 } : null),
      },
      async (host) => {
        expect((await call(`${host.base}/shl`, { method: 'POST', body: {}, headers: { 'x-api-key': 'let-me-in' } })).status).toBe(201);
        expect((await call(`${host.base}/shl`, { method: 'POST', body: {}, headers: { 'x-api-key': 'nope' } })).status).toBe(401);
        expect((await makeLink(host, {}, ADMIN)).status).toBe(401); // the bearer token is not what it asked for
      },
    ));

  it('is a host that is not started without the means to say', async () => {
    await expect(startHost({ adminToken: undefined as unknown as string })).rejects.toThrow(/needs a way to tell who may make links/);
    await expect(startHost({ adminToken: 'short' })).rejects.toThrow(/a token is a secret of 16 characters or more/);
    await expect(startHost({ managementAllow: ['10.0.0.0/33'] })).rejects.toThrow(/not an address or a range/);
    await expect(startHost({ managementAllow: ['ten dot zero'] })).rejects.toThrow(/not an address or a range/);
    await expect(
      startHost({ adminToken: undefined as unknown as string, clients: [{ name: 'a', token: 'a-token-of-sixteen-chars', allow: ['nope'] }] }),
    ).rejects.toThrow(/client a: allow: nope is not an address/);
  });
});

describe('a link', () => {
  it('ends by itself, thirty days on unless it is told otherwise', () =>
    using(async (host) => {
      const made = (await makeLink(host)).json;
      expect(made.expiresAt).toBe(seconds(host) + 30 * DAY);
      expect((await makeLink(host, { expiresAt: seconds(host) + 366 * DAY })).status).toBe(400);
      expect((await makeLink(host, { expiresAt: seconds(host) + 364 * DAY })).status).toBe(201);
    }));

  it('holds the files it was allowed, of the types the host keeps', () =>
    using({ maxFilesPerLink: 2, allowedContentTypes: ['application/fhir+json', 'application/smart-health-card'] }, async (host) => {
      const link = (await makeLink(host)).json;
      const type = (name: string) => `?contentType=${encodeURIComponent(name)}`;
      expect((await putFile(host, link, SUMMARY, undefined, type('text/html'))).status).toBe(400); // a type the host does not keep
      expect((await putFile(host, link, SUMMARY, undefined, type('application/fhir+json;fhirVersion=4.0.1'))).status).toBe(201); // parameters do not make a different type
      expect((await putFile(host, link, SUMMARY, undefined, type('Application/SMART-health-card'))).status).toBe(201);
      const third = await putFile(host, link);
      expect(third.status).toBe(409);
      expect(third.json.error).toMatch(/up to 2 files/);
    }));
});

describe('the addresses links may be made from', () => {
  it('are the ones a host names, and a client can be tied to its own', async () => {
    const events: HostEvent[] = [];
    await using(
      {
        managementAllow: ['203.0.113.0/24', '2001:db8::/32', '198.51.100.7'],
        trustProxy: 1,
        adminToken: undefined as unknown as string,
        clients: [
          { name: 'ehr', token: 'ehr-token-of-sixteen-chars', allow: ['203.0.113.10'] },
          { name: 'portal', token: 'portal-token-of-sixteen' },
        ],
        onEvent: (event) => void events.push(event),
      },
      async (host) => {
        const from = (ip: string) => ({ 'x-forwarded-for': ip });
        const make = (token: string, ip: string) => call(`${host.base}/shl`, { method: 'POST', token, body: {}, headers: from(ip) });

        expect((await make('ehr-token-of-sixteen-chars', '203.0.113.10')).status).toBe(201);
        expect((await make('portal-token-of-sixteen', '198.51.100.7')).status).toBe(201); // a single address
        expect((await make('portal-token-of-sixteen', '2001:db8:1::5')).status).toBe(201); // a range of the other kind
        const outside = await make('portal-token-of-sixteen', '192.0.2.50');
        expect([outside.status, outside.json]).toEqual([403, { error: 'not allowed from this address' }]); // right token, wrong place
        expect((await make('portal-token-of-sixteen', '203.0.114.1')).status).toBe(403); // one past the end of the range

        const tied = await make('ehr-token-of-sixteen-chars', '203.0.113.99');
        expect([tied.status, tied.json]).toEqual([403, { error: 'this token is not accepted from this address' }]); // in the host's range, but not its client's
        expect(events.some((event) => event.type === 'client.refused' && event.client === 'ehr' && event.ip === '203.0.113.99')).toBe(true);

        // What a receiver calls is open to anyone, and so are the probes.
        const link = (await make('portal-token-of-sixteen', '203.0.113.5')).json;
        expect((await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' }, from('192.0.2.50'))).status).toBe(200);
        expect((await call(`${host.base}/healthz`, { headers: from('192.0.2.50') })).status).toBe(200);
        const owner = (ip: string) => call(`${host.base}/shl/${link.id}/access`, { token: link.manageToken, headers: from(ip) });
        expect((await owner('203.0.113.5')).status).toBe(200);
        expect((await owner('192.0.2.50')).status).toBe(403); // nor may the link's own token be used from outside
        expect((await call(`${host.base}/shl/${link.id}`, { method: 'DELETE', token: link.manageToken, headers: from('192.0.2.50') })).status).toBe(403);
      },
    );
  });
});

describe("the host's own code", () => {
  it('can refuse a link before it is made, and a recipient before they are given anything', async () => {
    const events: HostEvent[] = [];
    await using(
      {
        onEvent: (event) => void events.push(event),
        authorizeCreate: ({ client, options }) => {
          if (options.label === 'blocked') return false;
          if (options.label === 'teapot') return { allow: false, status: 418, error: 'no tea' };
          return client === 'admin';
        },
        authorizeManifest: ({ recipient, link }) => {
          if (recipient === 'broken') throw new Error('the directory is down');
          return recipient !== 'banned' && link.createdBy === 'admin';
        },
      },
      async (host) => {
        expect((await makeLink(host, { label: 'blocked' })).status).toBe(403);
        const teapot = await makeLink(host, { label: 'teapot' });
        expect([teapot.status, teapot.json]).toEqual([418, { error: 'no tea' }]);

        const link = await linkWithFile(host, { passcode: 'right', attempts: 3 });
        const ask = (recipient: string, passcode: string) => manifest(`${host.base}/shl/${link.id}`, { recipient, passcode });
        const banned = await ask('banned', 'wrong');
        expect([banned.status, banned.json]).toEqual([403, { error: 'not allowed' }]);
        expect((await ask('banned', 'wrong')).status).toBe(403);
        expect((await ask('Dr Okafor', 'wrong')).json.remainingAttempts).toBe(2); // being refused did not use an attempt up
        expect((await ask('Dr Okafor', 'right')).status).toBe(200);

        const failed = await ask('broken', 'right');
        expect(failed.status).toBe(500); // a hook that fails does not let anyone in
        const { access } = (await call(`${host.base}/shl/${link.id}/access`, { token: link.manageToken })).json;
        expect(access.map((entry: { recipient: string }) => entry.recipient)).toEqual(['Dr Okafor', 'Dr Okafor']); // and the refused were not let in
        expect(events.some((event) => event.type === 'manifest.denied' && event.reason === 'recipient' && event.recipient === 'banned')).toBe(true);
      },
    );
  });

  it('is told what happens, and what is logged says nothing that opens a link', async () => {
    const events: HostEvent[] = [];
    const lines: Array<Record<string, any>> = [];
    await using({ onEvent: (event) => void events.push(event), logger: (line) => void lines.push(line) }, async (host) => {
      const link = await linkWithFile(host, { passcode: 'open sesame', attempts: 2 });
      const url = `${host.base}/shl/${link.id}`;
      await manifest(url, { recipient: 'Dr Okafor', passcode: 'nope' });
      const files = (await manifest(url, { recipient: 'Dr Okafor', passcode: 'open sesame' })).json.files;
      const ticket = new URL(files[0].location).searchParams.get('ticket')!;
      expect((await call(files[0].location)).status).toBe(200);
      await manifest(url, { recipient: 'Dr Okafor', passcode: 'nope' });
      expect((await manifest(url, { recipient: 'Dr Okafor', passcode: 'open sesame' })).status).toBe(404); // two attempts, and both are used
      const second = await linkWithFile(host);
      await call(`${host.base}/shl/${second.id}`, { method: 'DELETE', token: second.manageToken });

      expect(events.map((event) => event.type)).toEqual([
        'link.created', 'file.stored',
        'manifest.denied', 'manifest.opened', 'file.served',
        'manifest.denied', 'link.ended',
        'link.created', 'file.stored', 'link.ended',
      ]);
      expect(events.filter((event) => event.type === 'link.ended').map((event) => event.reason)).toEqual(['attempts', 'revoked']);
      expect(events[0]!.id).toBe(link.id); // the host's own code is given the whole id

      const logged = JSON.stringify(lines);
      for (const secret of [link.id, link.manageToken, ADMIN, 'open sesame', ticket, 'private-health-detail', Buffer.from(link.key).toString('base64url'), second.manageToken]) {
        expect(logged.includes(secret), `the log holds ${secret.slice(0, 12)}...`).toBe(false);
      }
      const requests = lines.filter((line) => line.msg === 'request');
      expect(requests.length).toBeGreaterThanOrEqual(10);
      for (const line of requests) {
        expect(typeof line.ms).toBe('number');
        expect(line.status >= 200 && line.route && line.requestId && line.ip, JSON.stringify(line)).toBeTruthy();
        if (line.link) expect(line.link).toHaveLength(8); // a link is told by its first eight characters
      }
      expect(requests.some((line) => line.route === 'manifest' && line.status === 401 && line.link === link.id.slice(0, 8))).toBe(true);
      expect(requests.every((line) => !('url' in line) && !('path' in line))).toBe(true); // no request line has an address in it
      expect(lines.some((line) => line.msg === 'link.ended' && line.reason === 'attempts')).toBe(true);
      expect(requests.some((line) => line.client === 'admin')).toBe(true); // and who made a link
    });
  });

  it('is answered, and logged with where it was, when it fails in the middle of a request', async () => {
    const lines: Array<Record<string, any>> = [];
    await using(
      {
        adminToken: undefined as unknown as string,
        authenticate: () => {
          throw new Error('the directory is down');
        },
        logger: (line) => void lines.push(line),
      },
      async (host) => {
        const failed = await Promise.race([makeLink(host), new Promise<{ status: string }>((resolve) => setTimeout(() => resolve({ status: 'hung' }), 3000))]);
        expect(failed.status).toBe(500); // an answer, and not a hang
        expect((failed as { json: unknown }).json).toEqual({ error: 'something went wrong' }); // which does not say what went wrong
        const error = lines.find((line) => line.level === 'error')!;
        expect(error.error).toMatch(/the directory is down/);
        expect(error.route).toBe('create');
        expect((await call(`${host.base}/healthz`)).status).toBe(200); // and the host is still there
      },
    );
  });
});

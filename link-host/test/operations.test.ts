import { Hono } from 'hono';
import { describe, expect, it } from 'vitest';

import { createHost } from '../src/app.js';

import { memoryStore } from '../src/store/memory.js';
import type { Store } from '../src/types.js';
import {
  ADMIN,
  SUMMARY,
  call,
  linkWithFile,
  makeLink,
  manifest,
  open,
  rawRequest,
  readSome,
  startHost,
  using,
} from './support.js';

const seconds = (host: { now(): number }) => Math.floor(host.now() / 1000);
const PEER = /^(127\.0\.0\.1|::1)$/;

describe('what a host does to keep itself in order', () => {
  it('clears away what has ended, and lets who asked be read for a while', () =>
    using({ retainSeconds: 3600 }, async (host) => {
      const link = await linkWithFile(host, { expiresAt: seconds(host) + 100 });
      await manifest(`${host.base}/shl/${link.id}`, { recipient: 'Dr Okafor' });
      expect(await host.host.store.getFile(link.id, 'f0')).toBeTruthy();
      const live = await linkWithFile(host);
      host.advance(200);

      const counts = await host.host.sweep();
      expect([counts?.emptied, counts?.forgotten, counts?.links]).toEqual([1, 0, 1]);
      expect(await host.host.store.getFile(link.id, 'f0')).toBeNull(); // the encrypted file is deleted when the link ends
      const audit = await call(`${host.base}/shl/${link.id}/access`, { token: link.manageToken });
      expect(audit.status).toBe(200);
      expect(audit.json.active).toBe(false);
      expect(audit.json.access.map((entry: { recipient: string }) => entry.recipient)).toEqual(['Dr Okafor']);
      expect((await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' })).status).toBe(404);
      expect((await manifest(`${host.base}/shl/${live.id}`, { recipient: 'r' })).status).toBe(200); // a link in use is not touched

      host.advance(3700);
      await host.host.sweep();
      expect((await call(`${host.base}/shl/${link.id}/access`, { token: link.manageToken })).status).toBe(404); // and then the record goes too
    }));

  it('keeps who asked with where from and whether it worked, unless it is told not to', async () => {
    await using(async (host) => {
      const link = await linkWithFile(host, { passcode: 'right' });
      await manifest(`${host.base}/shl/${link.id}`, { recipient: 'Guesser', passcode: 'wrong' }, { 'user-agent': 'scanner/1.0' });
      await manifest(`${host.base}/shl/${link.id}`, { recipient: 'Dr Okafor', passcode: 'right' }, { 'user-agent': 'viewer/2.0' });
      await manifest(`${host.base}/shl/${link.id}`, { recipient: 'Nobody' });
      const { access } = (await call(`${host.base}/shl/${link.id}/access`, { token: link.manageToken })).json;
      expect(access.map(({ recipient, outcome, userAgent }: Record<string, string>) => [recipient, outcome, userAgent])).toEqual([
        ['Guesser', 'wrong-passcode', 'scanner/1.0'],
        ['Dr Okafor', 'opened', 'viewer/2.0'],
      ]); // a request with no passcode is not a guess, and is not listed
      expect(access[0].ip).toMatch(PEER);
      expect(Number.isInteger(access[0].at)).toBe(true);
    });
    await using({ recordClientInfo: false }, async (quiet) => {
      const other = await linkWithFile(quiet);
      await manifest(`${quiet.base}/shl/${other.id}`, { recipient: 'Dr Okafor' }, { 'user-agent': 'viewer/2.0' });
      const kept = (await call(`${quiet.base}/shl/${other.id}/access`, { token: other.manageToken })).json.access[0];
      expect(Object.keys(kept).sort()).toEqual(['at', 'outcome', 'recipient']); // no address and no agent
    });
  });

  it('counts a caller by the address a proxy saw, and only when it is told there is one', async () => {
    const through = (host: { base: string }, link: { id: string }, forwarded?: string) =>
      manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' }, forwarded ? { 'x-forwarded-for': forwarded } : {});
    const seen = async (host: { base: string }, link: { id: string; manageToken: string }) =>
      (await call(`${host.base}/shl/${link.id}/access`, { token: link.manageToken })).json.access.map((entry: { ip: string }) => entry.ip);

    await using(async (open) => {
      const a = await linkWithFile(open);
      await through(open, a, '6.6.6.6');
      expect((await seen(open, a))[0]).toMatch(PEER); // a header from a caller no proxy vouches for is not believed
    });
    await using({ trustProxy: 1 }, async (proxied) => {
      const b = await linkWithFile(proxied);
      await through(proxied, b, '9.9.9.9, 203.0.113.9');
      await through(proxied, b, '2001:db8::7');
      await through(proxied, b, 'not an address; drop table');
      await through(proxied, b);
      expect((await seen(proxied, b)).map((ip: string) => (PEER.test(ip) ? 'peer' : ip))).toEqual(['203.0.113.9', '2001:db8::7', 'peer', 'peer']);
    });
    await using({ trustProxy: 2 }, async (twice) => {
      const c = await linkWithFile(twice);
      await through(twice, c, '7.7.7.7, 198.51.100.4, 10.0.0.2');
      expect(await seen(twice, c)).toEqual(['198.51.100.4']); // two proxies
    });
  });
});

describe('how often a host may be asked', () => {
  it('is only so often by one address, which is told to wait and may ask again when the time is up', () =>
    using({ rateLimits: { public: 5 } }, async (host) => {
      const link = await linkWithFile(host); // that called only what an owner calls
      const ask = () => manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' });
      for (let index = 0; index < 5; index += 1) expect((await ask()).status).toBe(200);
      const refused = await ask();
      expect(refused.status).toBe(429);
      expect(refused.json.error).toBe('too many requests');
      expect(Number(refused.headers.get('retry-after'))).toBeGreaterThanOrEqual(1);
      expect((await call(`${host.base}/shl/${'z'.repeat(43)}`)).status).toBe(429); // a guess at an address counts too
      expect((await call(`${host.base}/healthz`)).status).toBe(200); // a probe is never limited
      host.advance(61);
      expect((await ask()).status).toBe(200);
    }));

  it('is only so often for one link, from anywhere, and another link is not slowed by it', () =>
    using({ rateLimits: { perLink: 3 } }, async (host) => {
      const busy = await linkWithFile(host);
      const calm = await linkWithFile(host);
      const ask = (link: { id: string }) => manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' });
      for (let index = 0; index < 3; index += 1) expect((await ask(busy)).status).toBe(200);
      expect((await ask(busy)).status).toBe(429);
      expect((await ask(calm)).status).toBe(200);
    }));

  it('has limits of its own for making links, and for calling as an owner', () =>
    using({ rateLimits: { create: 2, management: 4 } }, async (host) => {
      expect((await makeLink(host)).status).toBe(201);
      expect((await makeLink(host)).status).toBe(201);
      expect((await makeLink(host)).status).toBe(429); // a client makes only so many links in a minute
      host.advance(61);
      const link = (await makeLink(host)).json;
      // Making that link was the first call of this window, so three more reach the limit of four.
      for (let index = 0; index < 3; index += 1) {
        expect((await call(`${host.base}/shl/${link.id}/access`, { token: link.manageToken })).status).toBe(200);
      }
      expect((await call(`${host.base}/shl/${link.id}/access`, { token: link.manageToken })).status).toBe(429);
    }));

  it('shuts out a caller who keeps getting a token wrong, even with the right one, until the time is up', () =>
    using({ rateLimits: { authFailures: 3 } }, async (host) => {
      const link = (await makeLink(host)).json;
      for (let index = 0; index < 3; index += 1) expect((await makeLink(host, {}, `guess-number-${index}-xxxxxx`)).status).toBe(401);
      expect((await makeLink(host, {}, ADMIN)).status).toBe(429); // the right token does not get through while it is barred
      expect((await call(`${host.base}/shl/${link.id}/access`, { token: link.manageToken })).status).toBe(429); // nor an owner's
      expect((await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' })).status).toBe(200); // a receiver, who has no token to get wrong, is not affected
      host.advance(301);
      expect((await makeLink(host, {}, ADMIN)).status).toBe(201);

      // A wrong token for a link counts like a wrong token for the host; a link that is not there does not.
      const other = (await makeLink(host)).json;
      for (let index = 0; index < 3; index += 1) await call(`${host.base}/shl/${other.id}/access`, { token: 'wrong' });
      expect((await call(`${host.base}/shl/${other.id}/access`, { token: other.manageToken })).status).toBe(429);
      host.advance(301);
      for (let index = 0; index < 10; index += 1) await call(`${host.base}/shl/${'q'.repeat(43)}/access`, { token: 'wrong' });
      expect((await makeLink(host, {}, ADMIN)).status).toBe(201); // asking after a link that does not exist is not a failed login
    }));

  it('is told to come back by a host that is full, which still answers its probes, and one slow upload does not hold up the rest', async () => {
    await using({ maxConcurrent: 2 }, async (host) => {
      const sockets: ReturnType<typeof rawRequest>[] = [];
      try {
        const link = (await makeLink(host)).json;
        const stalled = () =>
          rawRequest(
            host.port,
            [`POST /shl/${link.id}/file HTTP/1.1`, `Host: 127.0.0.1:${host.port}`, `Authorization: Bearer ${link.manageToken}`, 'Content-Length: 500', '', 'only-a-little'].join('\r\n'),
          );

        // One upload that never finishes: asking for the link's manifest still works.
        sockets.push(stalled());
        await new Promise((resolve) => setTimeout(resolve, 100));
        const fast = await Promise.race([
          manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' }),
          new Promise<{ status: string }>((resolve) => setTimeout(() => resolve({ status: 'waited' }), 2000)),
        ]);
        expect(fast.status).toBe(200); // the link was not held while the file was being sent

        // A second does fill the host, which is allowed two requests at once.
        sockets.push(stalled());
        await new Promise((resolve) => setTimeout(resolve, 100));
        const turned = await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' });
        expect(turned.status).toBe(503);
        expect(turned.headers.get('retry-after')).toBe('1');
        expect((await call(`${host.base}/healthz`)).status).toBe(200);
        for (const socket of sockets) socket.destroy();
        await new Promise((resolve) => setTimeout(resolve, 200));
        expect((await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' })).status).toBe(200); // and when they go it takes requests again
      } finally {
        for (const socket of sockets) socket.destroy();
      }
    });
  });

  it('refuses a request that says it is too large before reading it, and cuts off a body that is too large', () =>
    using({ maxFileBytes: 2000 }, async (host) => {
      const link = (await makeLink(host)).json;
      const head = [
        `POST /shl/${link.id}/file HTTP/1.1`,
        `Host: 127.0.0.1:${host.port}`,
        `Authorization: Bearer ${link.manageToken}`,
        'Content-Length: 99999999',
        '',
        '',
      ].join('\r\n');
      const socket = rawRequest(host.port, head);
      const reply = await readSome(socket);
      socket.destroy();
      expect(reply).toMatch(/^HTTP\/1\.1 413/); // refused on the strength of the header alone

      const big = await call(`${host.base}/shl/${link.id}`, { method: 'POST', body: JSON.stringify({ recipient: 'r', pad: 'x'.repeat(5000) }) });
      expect(big.status).toBe(413); // a manifest request is small
    }));

  it('cuts off a connection that is slow to speak, and does not read a header that is too long', () =>
    using({ headersTimeoutSeconds: 1, requestTimeoutSeconds: 30 }, async (host) => {
      const slow = rawRequest(host.port, `GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:${host.port}\r\n`);
      slow.resume(); // a socket nobody reads from never notices that the other end has gone
      const closed = await new Promise<boolean>((resolve) => {
        const timer = setTimeout(() => resolve(false), 8000);
        slow.once('close', () => {
          clearTimeout(timer);
          resolve(true);
        });
      });
      slow.destroy();
      expect(closed, 'a request whose headers never finish was dropped').toBe(true);

      const long = await call(`${host.base}/healthz`, { headers: { 'x-pad': 'x'.repeat(9000) } }).catch((error) => ({ status: error.cause?.code ?? 'refused' }));
      expect(long.status === 431 || typeof long.status === 'string', `a header of nine kilobytes is not taken (${long.status})`).toBe(true);
    }));
});

describe('what a host says about itself', () => {
  it('says what each answer is, and tells a browser not to keep it or guess at it', async () => {
    await using(async (plain) => {
      const link = await linkWithFile(plain);
      const files = (await manifest(`${plain.base}/shl/${link.id}`, { recipient: 'r' })).json.files;
      const first = await call(files[0].location);
      const second = await call(`${plain.base}/healthz`);
      for (const reply of [first, second, await call(`${plain.base}/nowhere`)]) {
        expect(reply.headers.get('cache-control')).toBe('no-store');
        expect(reply.headers.get('x-content-type-options')).toBe('nosniff');
        expect(reply.headers.get('referrer-policy')).toBe('no-referrer');
        expect(reply.headers.get('content-security-policy')).toMatch(/default-src 'none'/);
      }
      expect(first.headers.get('content-type')).toBe('application/jose');
      expect(first.headers.get('x-request-id')).not.toBe(second.headers.get('x-request-id'));
      expect(second.headers.get('strict-transport-security')).toBeNull(); // plain http says nothing of https
    });
    await using({ publicUrl: 'https://links.example.org' }, async (secure) => {
      expect((await call(`${secure.base}/healthz`)).headers.get('strict-transport-security')).toMatch(/max-age=\d+/);
    });
  });

  it("leaves it to the host which pages may call it, separately for receivers and for owners", () =>
    using({ corsOrigins: ['https://viewer.example'], managementCorsOrigins: ['https://app.example'] }, async (host) => {
      const link = (await makeLink(host)).json;
      const id = `${host.base}/shl/${link.id}`;
      const originOf = (reply: { headers: Headers }) => reply.headers.get('access-control-allow-origin');
      const from = (origin: string) => ({ origin });

      const viewer = await manifest(id, { recipient: 'r' }, from('https://viewer.example'));
      expect(originOf(viewer)).toBe('https://viewer.example');
      expect(viewer.headers.get('vary')).toBe('Origin'); // an answer that depends on who asked says so
      expect(originOf(await manifest(id, { recipient: 'r' }, from('https://other.example')))).toBeNull();

      const app = await call(`${host.base}/shl`, { method: 'POST', token: ADMIN, body: {}, headers: from('https://app.example') });
      expect(originOf(app)).toBe('https://app.example');
      expect(originOf(await call(`${host.base}/shl`, { method: 'POST', token: ADMIN, body: {}, headers: from('https://viewer.example') }))).toBeNull(); // a receiver's page may not make links

      const preflight = (origin: string, method: string) => call(id, { method: 'OPTIONS', headers: { origin, 'access-control-request-method': method } });
      expect(originOf(await preflight('https://viewer.example', 'POST'))).toBe('https://viewer.example');
      expect(originOf(await preflight('https://viewer.example', 'DELETE'))).toBeNull(); // switching a link off is an owner's call
      expect(originOf(await preflight('https://app.example', 'DELETE'))).toBe('https://app.example');
      expect((await preflight('https://evil.example', 'POST')).status).toBe(204); // a refusal is the absence of permission, not an error
    }));

  it('lets a balancer tell whether it is up, whether it can take work, and when it is going', async () => {
    const store = memoryStore();
    let working = true;
    const original = store.ping.bind(store);
    store.ping = async () => {
      if (!working) throw new Error('the database is full');
      return original();
    };
    await using({ store, rateLimits: { public: 1 } }, async (host) => {
      expect((await call(`${host.base}/readyz`)).json).toEqual({ status: 'ready' });
      working = false;
      expect((await call(`${host.base}/readyz`)).json).toEqual({ status: 'unavailable' }); // a host that cannot write is not ready
      expect((await call(`${host.base}/healthz`)).status).toBe(200); // though it is still up
      working = true;
      for (let index = 0; index < 5; index += 1) expect((await call(`${host.base}/readyz`)).status).toBe(200); // probes are not limited

      host.host.drain();
      expect((await call(`${host.base}/readyz`)).json).toEqual({ status: 'draining' });
      expect((await call(`${host.base}/healthz`)).status).toBe(200);
      const refused = await manifest(`${host.base}/shl/${'a'.repeat(43)}`, { recipient: 'r' });
      expect(refused.status).toBe(503);
      expect(refused.headers.get('retry-after')).toBe('1');
    });
  });

  it('tells its counts to whoever holds the metrics token, and to no one if there is none', async () => {
    await using(async (none) => {
      expect((await call(`${none.base}/metrics`, { token: 'metrics-token-of-sixteen' })).status).toBe(404); // a host with no token has no page
    });
    await using({ metricsToken: 'metrics-token-of-sixteen' }, async (host) => {
      expect((await call(`${host.base}/metrics`)).status).toBe(401);
      expect((await call(`${host.base}/metrics`, { token: ADMIN })).status).toBe(401); // the admin token is not the metrics token

      const link = await linkWithFile(host, { passcode: 'p' });
      await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r', passcode: 'p' });
      await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r', passcode: 'wrong' });
      await call(`${host.base}/not/a/page/${'secret'.repeat(5)}`);
      const report = await call(`${host.base}/metrics`, { token: 'metrics-token-of-sixteen' });
      expect(report.status).toBe(200);
      expect(report.headers.get('content-type')).toMatch(/^text\/plain/);
      const lines = report.text.split('\n');
      for (const line of [
        'shl_links_created_total 1',
        'shl_files_stored_total 1',
        'shl_manifest_total{outcome="opened"} 1',
        'shl_manifest_total{outcome="wrong_passcode"} 1',
        'shl_requests_total{route="create",status="201"} 1',
        'shl_requests_total{route="unknown",status="404"} 1',
      ]) {
        expect(lines, report.text).toContain(line);
      }
      expect(report.text).not.toContain('secret'); // a series is named for a route, never for what was asked
      expect(report.text).not.toContain(link.id);
    });
  });
});

describe('where a host keeps its links', () => {
  it('is any store that keeps the promises of one', async () => {
    // A store that is only what the interface says, so that nothing else is asked of one.
    const inner = memoryStore();
    const store: Store = {
      create: (link) => inner.create(link),
      get: (id) => inner.get(id),
      update: (id, change) => inner.update(id, change),
      getFile: (id, name) => inner.getFile(id, name),
      listAccess: (id) => inner.listAccess(id),
      secret: (name) => inner.secret(name),
      sweep: (args) => inner.sweep(args),
      ping: () => inner.ping(),
      close: () => inner.close(),
    };
    await using({ store }, async (host) => {
      const link = await linkWithFile(host, { passcode: 'p' });
      const files = (await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r', passcode: 'p' })).json.files;
      expect(open((await call(files[0].location)).text, link.key)).toBe(SUMMARY);
      expect((await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r', passcode: 'x' })).status).toBe(401);
      expect((await call(`${host.base}/shl/${link.id}`, { method: 'DELETE', token: link.manageToken })).status).toBe(204);
      expect((await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r', passcode: 'p' })).status).toBe(404);
    });
  });

  it('can be mounted inside a larger Hono app, and asked of without a socket', async () => {
    const host = await createHost({ store: memoryStore(), adminToken: ADMIN, sweepIntervalSeconds: 0, remoteAddress: () => '203.0.113.9' });
    try {
      const parent = new Hono();
      parent.get('/', (c) => c.text('the parent'));
      parent.route('/', host.app);
      expect(await (await parent.request('/')).text()).toBe('the parent');
      const made = await parent.request('/shl', { method: 'POST', headers: { authorization: `Bearer ${ADMIN}` }, body: '{}' });
      expect(made.status).toBe(201);
      const link = (await made.json()) as { id: string; manageToken: string };
      const asked = await parent.request(`/shl/${link.id}`, { method: 'POST', body: JSON.stringify({ recipient: 'r' }) });
      expect(asked.status).toBe(200);
      const seen = await parent.request(`/shl/${link.id}/access`, { headers: { authorization: `Bearer ${link.manageToken}` } });
      expect(((await seen.json()) as { access: Array<{ ip: string }> }).access[0]?.ip).toBe('203.0.113.9'); // the address the host was told of
      expect((await parent.request('/readyz')).status).toBe(200);
    } finally {
      host.close();
    }
  });

  it('is asked for the ticket secret once, so that every process signs alike', async () => {
    const store = memoryStore();
    const first = await startHost({ store });
    const link = await linkWithFile(first);
    const location = (await manifest(`${first.base}/shl/${link.id}`, { recipient: 'r' })).json.files[0].location as string;
    // Another process on the same database: it has the secret the first made, so it honours the ticket.
    const second = await startHost({ store });
    try {
      const there = new URL(location);
      expect((await call(`${second.base}${there.pathname}${there.search}`)).status).toBe(200);
    } finally {
      await second.stop();
      await first.stop();
    }
  });
});

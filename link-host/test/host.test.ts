import { describe, expect, it } from 'vitest';

import {
  ADMIN,
  SUMMARY,
  call,
  encryptFile,
  linkWithFile,
  makeLink,
  manifest,
  open,
  randomKey,
  using,
} from './support.js';

describe('a link host', () => {
  it("lets only the admin make links, and only the link's own token put a file on it", () =>
    using(async (host) => {
      expect((await call(`${host.base}/shl`, { method: 'POST', body: {} })).status).toBe(401);
      expect((await call(`${host.base}/shl`, { method: 'POST', token: 'wrong', body: {} })).status).toBe(401);
      const first = (await makeLink(host)).json;
      const second = (await makeLink(host)).json;
      expect(first.id).not.toBe(second.id);
      expect(first.id).toHaveLength(43); // 256 bits of address
      const file = await encryptFile(SUMMARY, randomKey());
      const put = (id: string, token?: string) => call(`${host.base}/shl/${id}/file`, { method: 'POST', ...(token ? { token } : {}), body: file });
      expect((await put(first.id)).status).toBe(401);
      expect((await put(first.id, second.manageToken)).status).toBe(401); // another link's token does not do
      expect((await put(first.id, ADMIN)).status).toBe(401); // nor does the admin token
      expect((await put(first.id, first.manageToken)).status).toBe(201);
    }));

  it('refuses a file that is not encrypted, or is too large, and keeps nothing', () =>
    using({ maxFileBytes: 2000 }, async (host) => {
      const made = (await makeLink(host)).json;
      const put = (body: string, query = '') => call(`${host.base}/shl/${made.id}/file${query}`, { method: 'POST', token: made.manageToken, body });
      const plain = await put(SUMMARY);
      expect(plain.status).toBe(400);
      expect(plain.json.error).toMatch(/only encrypted files/);
      expect((await put('a.b.c.d.e')).status).toBe(400);
      const header = Buffer.from(JSON.stringify({ alg: 'RSA-OAEP', enc: 'A256GCM' })).toString('base64url');
      expect((await put(`${header}.x.y.z.w`)).status).toBe(400); // a key wrapped for someone else is not what a link holds
      const real = await encryptFile('x', randomKey());
      expect((await put(real, '?contentType=not%20a%20type')).status).toBe(400);
      expect((await put('x'.repeat(5000))).status).toBe(413);
      const now = await manifest(`${host.base}/shl/${made.id}`, { recipient: 'r' });
      expect(now.json.files).toEqual([]); // nothing was stored
    }));

  it('counts a passcode, does not count an absent one, and switches the link off when the attempts run out', () =>
    using(async (host) => {
      const link = await linkWithFile(host, { passcode: 'open sesame', attempts: 3 });
      const ask = (passcode?: string) => manifest(`${host.base}/shl/${link.id}`, { recipient: 'r', ...(passcode ? { passcode } : {}) });
      const none = await ask();
      expect([none.status, none.json]).toEqual([401, { remainingAttempts: 3 }]);
      const wrongOnce = await ask('nope');
      expect([wrongOnce.status, wrongOnce.json]).toEqual([401, { remainingAttempts: 2 }]);
      expect((await ask('open sesame')).status).toBe(200); // the right passcode still works
      expect((await ask('nope')).json).toEqual({ remainingAttempts: 1 });
      expect((await ask('nope')).json).toEqual({ remainingAttempts: 0 });
      expect((await ask('open sesame')).status).toBe(404); // and now not even the right one
      expect(await host.host.store.getFile(link.id, 'f0')).toBeNull(); // the files went with the link
      const stored = JSON.stringify(await host.host.store.get(link.id));
      expect(stored).not.toContain('open sesame'); // the passcode is stored hashed
    }));

  it('counts guesses that arrive together, each', () =>
    using(async (host) => {
      const link = await linkWithFile(host, { passcode: 'right', attempts: 5 });
      const replies = await Promise.all(
        Array.from({ length: 12 }, () => manifest(`${host.base}/shl/${link.id}`, { recipient: 'r', passcode: 'wrong' })),
      );
      const refused = replies.filter((reply) => reply.status === 401);
      expect(refused).toHaveLength(5); // five guesses, and not one more
      expect(refused.map((reply) => reply.json.remainingAttempts).sort()).toEqual([0, 1, 2, 3, 4]);
      expect(replies.filter((reply) => reply.status === 404)).toHaveLength(7);
    }));

  it('hands a file out only to someone holding the ticket the manifest gave, for a minute', () =>
    using(async (host) => {
      const link = await linkWithFile(host);
      const location: string = (await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' })).json.files[0].location;
      expect((await call(location)).status).toBe(200);
      const bare = location.split('?')[0]!;
      expect((await call(bare)).status).toBe(404); // no ticket
      expect((await call(`${bare}?ticket=1.AAAA`)).status).toBe(404); // a made-up ticket
      const [expires, mac] = new URL(location).searchParams.get('ticket')!.split('.') as [string, string];
      expect((await call(`${bare}?ticket=${Number(expires) + 1000}.${mac}`)).status).toBe(404); // a ticket with a later expiry
      const other = await linkWithFile(host);
      expect((await call(location.replace(link.id, other.id))).status).toBe(404); // a ticket for another link
      host.advance(59);
      expect((await call(location)).status).toBe(200);
      host.advance(5);
      expect((await call(location)).status).toBe(404); // a minute later it is spent
      const again = (await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' })).json.files[0].location;
      expect((await call(again)).status).toBe(200); // and asking again gives a new one
    }));

  it('answers for a link that has expired, or been switched off, as if it were not there', () =>
    using(async (host) => {
      const soon = await linkWithFile(host, { expiresAt: Math.floor(host.now() / 1000) + 100 });
      expect((await manifest(`${host.base}/shl/${soon.id}`, { recipient: 'r' })).status).toBe(200);
      host.advance(200);
      expect((await manifest(`${host.base}/shl/${soon.id}`, { recipient: 'r' })).status).toBe(404);

      const link = await linkWithFile(host);
      const location = (await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' })).json.files[0].location;
      expect((await call(`${host.base}/shl/${link.id}`, { method: 'DELETE' })).status).toBe(401);
      expect((await call(`${host.base}/shl/${link.id}`, { method: 'DELETE', token: link.manageToken })).status).toBe(204);
      expect((await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' })).status).toBe(404);
      expect((await call(location)).status).toBe(404); // a ticket already given does not outlive the link
      expect(await host.host.store.getFile(link.id, 'f0')).toBeNull();
      expect((await call(`${host.base}/shl/${link.id}/file`, { method: 'POST', token: link.manageToken, body: 'x' })).status).toBe(404);
      expect((await makeLink(host, { expiresAt: 5 })).status).toBe(400); // a link cannot start out expired
    }));

  it('gives the file itself for the U flag: no passcode, and one file', () =>
    using(async (host) => {
      const link = await linkWithFile(host);
      expect((await call(`${host.base}/shl/${link.id}`)).status).toBe(400); // who is asking is required
      const direct = await call(`${host.base}/shl/${link.id}?recipient=Dr%20Okafor`);
      expect(direct.status).toBe(200);
      expect(direct.headers.get('content-type')).toBe('application/jose');
      expect(open(direct.text, link.key)).toBe(SUMMARY);
      const locked = await linkWithFile(host, { passcode: 'p' });
      expect((await call(`${host.base}/shl/${locked.id}?recipient=r`)).status).toBe(404); // a passcode link is never given away by address
      const empty = (await makeLink(host)).json;
      expect((await call(`${host.base}/shl/${empty.id}?recipient=r`)).status).toBe(404);
    }));

  it("keeps who has asked, for the link's own token to read", () =>
    using(async (host) => {
      const link = await linkWithFile(host);
      await manifest(`${host.base}/shl/${link.id}`, { recipient: 'Dr Okafor' });
      await call(`${host.base}/shl/${link.id}?recipient=Nurse%20Ada`);
      expect((await call(`${host.base}/shl/${link.id}/access`)).status).toBe(401);
      const log = (await call(`${host.base}/shl/${link.id}/access`, { token: link.manageToken })).json.access;
      expect(log.map((entry: { recipient: string }) => entry.recipient)).toEqual(['Dr Okafor', 'Nurse Ada']);
      expect((await manifest(`${host.base}/shl/${link.id}`, {})).status).toBe(400); // a manifest request says who it is from
    }));

  it('sends a file small enough inside the manifest when the receiver asks for that', () =>
    using(async (host) => {
      const link = await linkWithFile(host);
      const ask = (embeddedLengthMax: number) => manifest(`${host.base}/shl/${link.id}`, { recipient: 'r', embeddedLengthMax });
      const inline = (await ask(1_000_000)).json.files[0];
      expect(inline.embedded).toBeTruthy();
      expect(inline.location).toBeUndefined();
      expect(open(inline.embedded, link.key)).toBe(SUMMARY);
      const apart = (await ask(10)).json.files[0];
      expect(apart.location).toBeTruthy();
      expect(apart.embedded).toBeUndefined();
    }));

  it('reaches nothing for a path that is not a link address, and lets any origin call what a receiver calls', () =>
    using(async (host) => {
      for (const bad of ['/shl/..%2F..%2Fetc%2Fpasswd', '/shl/short', '/shl/../x', '/other', `/shl/${'a'.repeat(43)}`, `/shl/${'a'.repeat(43)}/nothing`]) {
        const reply = await call(`${host.base}${bad}`, { method: 'POST', body: { recipient: 'r' } });
        expect(reply.status, bad).toBe(404);
      }
      // What a receiver calls can be called from a page on any origin, so a viewer in a browser works.
      const asked = await call(`${host.base}/shl/${'a'.repeat(43)}`, {
        method: 'OPTIONS',
        headers: { origin: 'https://viewer.example', 'access-control-request-method': 'POST' },
      });
      expect(asked.status).toBe(204);
      expect(asked.headers.get('access-control-allow-origin')).toBe('*');
      expect(asked.headers.get('access-control-allow-headers')).toMatch(/authorization/);
      // What the owner of a link calls is not, unless the host is told which origins to trust.
      const made = await call(`${host.base}/shl`, { method: 'POST', token: ADMIN, body: {}, headers: { origin: 'https://evil.example' } });
      expect(made.status).toBe(201);
      expect(made.headers.get('access-control-allow-origin')).toBeNull();
      expect(made.headers.get('cache-control')).toBe('no-store');
      expect((await call(`${host.base}/healthz`)).status).toBe(200);
    }));
});

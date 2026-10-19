import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import { describe, expect, it } from 'vitest';

import {
  ADMIN,
  call,
  databaseUrl,
  linkWithFile,
  makeLink,
  manifest,
  runCli,
  scratchDatabase,
  startCli,
  tempFile,
} from './support.js';

const BASE_ENV = { ADMIN_TOKEN: ADMIN, HOST: '127.0.0.1', STORE: 'memory' };

describe('the command', () => {
  it('says how to use it, and makes a token together with the hash that stands for it', async () => {
    const help = await runCli(['--help'], {});
    expect(help.stdout).toMatch(/Usage: refhir-link-host/);
    expect(help.stdout).toMatch(/new-token/);
    expect(help.stdout).toMatch(/migrate/);
    expect((await runCli(['--version'], {})).stdout.trim()).toMatch(/^\d+\.\d+\.\d+/);

    const made = await runCli(['new-token'], {});
    const token = made.stdout.match(/token:\s+(\S+)/)![1]!;
    const hash = made.stdout.match(/tokenHash:\s+(sha256:[0-9a-f]{64})/)![1]!;
    expect(hash).toBe(`sha256:${createHash('sha256').update(token).digest('hex')}`);
    expect(token.length).toBeGreaterThanOrEqual(43); // 256 bits

    // The hash in a clients file is enough for the host to know the token, and the host keeps no more.
    const clients = tempFile('clients.json', JSON.stringify({ clients: [{ name: 'ehr', tokenHash: hash, maxLifetimeDays: 3 }] }));
    const host = await startCli({ ADMIN_TOKEN: '', CLIENTS_FILE: clients.file });
    try {
      const link = await makeLink({ base: host.base }, {}, token);
      expect(link.status).toBe(201);
      expect(link.json.expiresAt - Math.floor(Date.now() / 1000)).toBeLessThanOrEqual(3 * 86_400 + 5); // the client's own limit
      expect((await makeLink({ base: host.base }, {}, ADMIN)).status).toBe(401);
      expect(host.output()).not.toContain(token); // the token is not in the log
      expect(host.lines().find((line) => line.msg === 'listening')?.clients).toEqual(['ehr']);
    } finally {
      await host.stop();
      clients.clean();
    }
  });

  it('says every way a host is set up wrongly, and does not start', async () => {
    const none = await runCli([], {});
    expect(none.code).toBe(1);
    expect(none.stderr).toMatch(/The link host cannot start:/);
    expect(none.stderr).toMatch(/ {2}- Set ADMIN_TOKEN to a secret of 16 characters or more/);
    expect(none.stderr).toMatch(/ {2}- Set PUBLIC_URL to the address this host is reached at/);
    expect(none.stderr).toMatch(/ {2}- Set DATABASE_URL to where the host keeps links/);

    const several = await runCli([], { ADMIN_TOKEN: 'short', PUBLIC_URL: 'http://links.example.org', STORE: 'memory', PORT: '0', RATE_LIMIT_PUBLIC: 'many' });
    expect(several.code).toBe(1);
    expect(several.stderr.split('\n').filter((line) => line.startsWith('  - '))).toHaveLength(4);
  });

  it('checks that a database can be reached, and says so when it cannot', async () => {
    const fine = await runCli(['--check'], BASE_ENV);
    expect(fine.stdout).toMatch(/settings are fine/);
    const lost = await runCli(['--check'], { ...BASE_ENV, STORE: 'postgres', DATABASE_URL: 'postgres://nobody:nothing@127.0.0.1:1/none', DATABASE_WAIT_SECONDS: '1' });
    expect(lost.code).toBe(1);
    expect(lost.stderr).toMatch(/The database cannot be used:/);
  });

  it('says what it does, in one JSON line each, and names a link by its first eight characters', async () => {
    const host = await startCli({ PUBLIC_URL: 'https://links.example.org' });
    try {
      const link = await linkWithFile({ base: host.base });
      await manifest(`${host.base}/shl/${link.id}`, { recipient: 'Dr Okafor' });
      await new Promise((resolve) => setTimeout(resolve, 100));
      const lines = host.lines();
      expect(lines.some((line) => line.msg === 'listening' && line.publicUrl === 'https://links.example.org' && line.store === 'memory')).toBe(true);
      expect(lines.some((line) => line.level === 'warn' && /TRUST_PROXY=1/.test(line.msg)), 'a proxy with no TRUST_PROXY is a thing to say').toBe(true);
      expect(lines.some((line) => line.level === 'warn' && /STORE=memory/.test(line.msg))).toBe(true);
      const requests = lines.filter((line) => line.msg === 'request');
      expect(requests.some((line) => line.route === 'manifest' && line.status === 200 && line.link === link.id.slice(0, 8))).toBe(true);
      expect(host.output()).not.toContain(link.id);
      expect(host.output()).not.toContain(link.manageToken);
      expect(host.output().split('\n').filter(Boolean).every((line) => JSON.parse(line).ts)).toBe(true);
    } finally {
      await host.stop();
    }
    const quiet = await startCli({ LOG_REQUESTS: 'false', PUBLIC_URL: 'http://127.0.0.1:1' });
    try {
      await call(`${quiet.base}/healthz`);
      await new Promise((resolve) => setTimeout(resolve, 100));
      expect(quiet.lines().filter((line) => line.msg === 'request')).toEqual([]); // requests are not logged when it is asked not to
    } finally {
      await quiet.stop();
    }
  });

  it('says it is not ready when told to stop, goes on answering for a moment, then finishes and goes', async () => {
    const host = await startCli({ SHUTDOWN_DELAY_SECONDS: '1', SHUTDOWN_GRACE_SECONDS: '5' });
    try {
      const link = await linkWithFile({ base: host.base });
      host.child.kill('SIGTERM');
      await new Promise((resolve) => setTimeout(resolve, 300));
      const ready = await call(`${host.base}/readyz`);
      expect([ready.status, ready.json]).toEqual([503, { status: 'draining' }]);
      expect((await call(`${host.base}/healthz`)).status).toBe(200);
      expect((await manifest(`${host.base}/shl/${link.id}`, { recipient: 'r' })).status).toBe(503); // new work goes elsewhere
      expect(await host.exited).toEqual({ code: 0, signal: null });
      expect(host.lines().some((line) => line.msg === 'stopping' && line.signal === 'SIGTERM')).toBe(true);
    } finally {
      await host.stop();
    }
  });
});

describe('the command, with a SQLite file', () => {
  const inTemp = async (test: (dir: string, env: Record<string, string>) => Promise<void>) => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'link-host-cli-'));
    try {
      await test(dir, { STORE: '', DATABASE_URL: `sqlite://${dir}/links.db` });
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  };

  it('makes the file and the schema, and says it can be written', () =>
    inTemp(async (dir, env) => {
      expect((await runCli(['--check'], { ...BASE_ENV, ...env })).stdout).toMatch(/settings are fine/);
      expect(fs.existsSync(`${dir}/links.db`)).toBe(true);
      const refused = await runCli(['--check'], { ...BASE_ENV, ...env, DATABASE_URL: `sqlite://${dir}/fresh.db`, AUTO_MIGRATE: 'false' });
      expect(refused.code).toBe(1);
      expect(refused.stderr).toMatch(/the database has no schema: run "refhir-link-host migrate"/);
      expect((await runCli(['migrate'], { ...BASE_ENV, ...env, DATABASE_URL: `sqlite://${dir}/fresh.db` })).stdout).toMatch(/database is up to date/);
      expect((await runCli(['--check'], { ...BASE_ENV, ...env, DATABASE_URL: `sqlite://${dir}/fresh.db`, AUTO_MIGRATE: 'false' })).stdout).toMatch(/settings are fine/);
    }));

  it('copies the database to a new file while it runs, and says what it cannot back up', () =>
    inTemp(async (dir, env) => {
      const host = await startCli(env);
      try {
        const link = await linkWithFile({ base: host.base }, { passcode: 'p' });
        const copy = `${dir}/backups/copy.db`;
        const made = await runCli(['backup', copy], { ...BASE_ENV, ...env });
        expect(made.code, made.stderr).toBe(0);
        expect(made.stdout).toMatch(/A copy of the database is in/);
        expect(fs.existsSync(copy)).toBe(true);
        // The copy is a database a host can start on, with the link in it.
        const restored = await startCli({ ...env, DATABASE_URL: `sqlite://${copy}` });
        try {
          expect((await manifest(`${restored.base}/shl/${link.id}`, { recipient: 'r', passcode: 'p' })).status).toBe(200);
        } finally {
          await restored.stop();
        }
        const again = await runCli(['backup', copy], { ...BASE_ENV, ...env });
        expect(again.code).toBe(1);
        expect(again.stderr).toMatch(/The backup was not made: .* a backup is made to a new file/);
      } finally {
        await host.stop();
      }
      // A mistyped path is not a database to make: it is refused, and nothing is created.
      const typo = await runCli(['backup', `${dir}/b.db`], { ...BASE_ENV, ...env, DATABASE_URL: `sqlite://${dir}/linkz.db` });
      expect(typo.code).toBe(1);
      expect(typo.stderr).toMatch(/There is no database at .*linkz\.db to back up/);
      expect(fs.existsSync(`${dir}/linkz.db`)).toBe(false);
      expect(fs.existsSync(`${dir}/b.db`)).toBe(false);
      const nowhere = await runCli(['backup'], { ...BASE_ENV, ...env });
      expect(nowhere.stderr).toMatch(/Say where to put the copy/);
      const memory = await runCli(['backup', `${dir}/x.db`], BASE_ENV);
      expect(memory.code).toBe(1);
      expect(memory.stderr).toMatch(/backup is for a SQLite database/);
    }));

  it('keeps links across a restart, and shares them with a second process on the file, which counts a guess at a passcode once between them', () =>
    inTemp(async (_dir, env) => {
      const first = await startCli(env);
      const link = await linkWithFile({ base: first.base }, { passcode: 'right', attempts: 5 });
      const location = (await manifest(`${first.base}/shl/${link.id}`, { recipient: 'r', passcode: 'right' })).json.files[0].location as string;
      expect((await call(location)).status).toBe(200);
      expect(first.lines().find((line) => line.msg === 'listening')?.store).toBe('sqlite');
      expect(first.errors()).not.toMatch(/ExperimentalWarning/); // nothing on stderr that is not a line of JSON
      await first.stop();

      // A new process, and a second beside it, on the one file.
      const a = await startCli(env);
      const b = await startCli(env);
      try {
        const there = new URL(location);
        expect((await call(`${a.base}${there.pathname}${there.search}`)).status, 'a ticket given before the restart is good after it').toBe(200);
        expect((await call(`${b.base}${there.pathname}${there.search}`)).status, 'and to the other process').toBe(200);

        const replies = await Promise.all(
          Array.from({ length: 16 }, (_, index) => manifest(`${index % 2 ? a.base : b.base}/shl/${link.id}`, { recipient: 'r', passcode: 'wrong' })),
        );
        const refused = replies.filter((reply) => reply.status === 401);
        expect(refused).toHaveLength(5); // five guesses between the two of them, and not one more
        expect(refused.map((reply) => reply.json.remainingAttempts).sort()).toEqual([0, 1, 2, 3, 4]);
        expect(replies.filter((reply) => reply.status === 404)).toHaveLength(11);
        expect((await manifest(`${b.base}/shl/${link.id}`, { recipient: 'r', passcode: 'right' })).status).toBe(404); // the other process knows it is over
      } finally {
        await a.stop();
        await b.stop();
      }
    }), 60_000);
});

describe.skipIf(!databaseUrl)('the command, with a database', () => {
  it('migrates the database, once, and a host that is told not to will not start on one that is not', async () => {
    const database = await scratchDatabase();
    await database.create();
    try {
      const env = { ...BASE_ENV, STORE: 'postgres', DATABASE_URL: database.url };
      const refused = await runCli(['--check'], { ...env, AUTO_MIGRATE: 'false' });
      expect(refused.code).toBe(1);
      expect(refused.stderr).toMatch(/the database has no schema: run "refhir-link-host migrate"/);

      expect((await runCli(['migrate'], env)).stdout).toMatch(/database is up to date/);
      expect((await runCli(['migrate'], env)).stdout).toMatch(/database is up to date/); // again changes nothing
      const sql = database.connect();
      try {
        const [row] = await sql<{ n: number }[]>`SELECT count(*)::int AS n FROM schema_migrations`;
        expect(row?.n).toBe(1);
      } finally {
        await sql.end({ timeout: 1 });
      }
      expect((await runCli(['--check'], { ...env, AUTO_MIGRATE: 'false' })).stdout).toMatch(/settings are fine/);
    } finally {
      await database.done();
    }
  });

  it('waits for a database that is not there yet, when it is told to', async () => {
    const database = await scratchDatabase();
    try {
      const env = { ...BASE_ENV, STORE: 'postgres', DATABASE_URL: database.url, DATABASE_WAIT_SECONDS: '15' };
      const running = runCli(['--check'], env);
      await new Promise((resolve) => setTimeout(resolve, 1500));
      await database.create(); // it comes up while the host is waiting
      expect((await running).stdout).toMatch(/settings are fine/);
    } finally {
      await database.done();
    }
  });

  it('keeps links across a restart, and shares them with a second process, which counts a guess at a passcode once between them', async () => {
    const database = await scratchDatabase();
    await database.create();
    const env = { STORE: 'postgres', DATABASE_URL: database.url };
    try {
      const first = await startCli(env);
      const link = await linkWithFile({ base: first.base }, { passcode: 'right', attempts: 5 });
      const location = (await manifest(`${first.base}/shl/${link.id}`, { recipient: 'r', passcode: 'right' })).json.files[0].location as string;
      expect((await call(location)).status).toBe(200);
      await first.stop();

      // A new process, and a second beside it, on the one database.
      const a = await startCli(env);
      const b = await startCli(env);
      try {
        const there = new URL(location);
        expect((await call(`${a.base}${there.pathname}${there.search}`)).status, 'a ticket given before the restart is good after it').toBe(200);
        expect((await call(`${b.base}${there.pathname}${there.search}`)).status, 'and to the other process').toBe(200);

        const replies = await Promise.all(
          Array.from({ length: 16 }, (_, index) =>
            manifest(`${index % 2 ? a.base : b.base}/shl/${link.id}`, { recipient: 'r', passcode: 'wrong' }),
          ),
        );
        const refused = replies.filter((reply) => reply.status === 401);
        expect(refused).toHaveLength(5); // five guesses between the two of them, and not one more
        expect(refused.map((reply) => reply.json.remainingAttempts).sort()).toEqual([0, 1, 2, 3, 4]);
        expect(replies.filter((reply) => reply.status === 404)).toHaveLength(11);
        expect((await manifest(`${b.base}/shl/${link.id}`, { recipient: 'r', passcode: 'right' })).status).toBe(404); // the other process knows it is over
      } finally {
        await a.stop();
        await b.stop();
      }
    } finally {
      await database.done();
    }
  }, 60_000);
});

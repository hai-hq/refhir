import fs from 'node:fs';
import path from 'node:path';

import { afterAll, describe, expect, it } from 'vitest';

import { StoreBusy, type Store } from '../src/types.js';
import { ID, aLink, databaseUrl, memory, schemaStore, scratchDatabase, sqliteFile } from './support.js';

// Every store keeps the same promises, so one set of tests holds them all to it.
const stores: Array<[string, () => Promise<{ store: Store; done: () => Promise<unknown> }>]> = [
  ['memory', async () => memory()],
  [
    'sqlite',
    async () => {
      const sqlite = sqliteFile();
      return { store: await sqlite.open(), done: sqlite.done };
    },
  ],
  ...(databaseUrl
    ? [
        [
          'postgres',
          async () => {
            const schema = await schemaStore();
            return { store: await schema.open(), done: schema.done };
          },
        ] as [string, () => Promise<{ store: Store; done: () => Promise<unknown> }>],
      ]
    : []),
];

it('the Postgres tests have a database to run against, wherever they are expected to', () => {
  if (process.env.CI) expect(databaseUrl, 'CI sets LINK_HOST_TEST_DATABASE_URL').toBeTruthy();
});

describe.each(stores)('%s store', (_name, make) => {
  async function using(test: (store: Store) => Promise<void>) {
    const { store, done } = await make();
    try {
      await test(store);
    } finally {
      await done();
    }
  }
  const second = (store: Store, id: string) => store.update(id, async (link) => link?.attemptsLeft);

  it('keeps a link as it was left, and what throws keeps nothing', () =>
    using(async (store) => {
      expect(await store.get(ID('a'))).toBeNull();
      await store.create(aLink(ID('a'), { passcode: { salt: 'aa', hash: 'bb' } }));
      await expect(store.create(aLink(ID('a')))).rejects.toThrow();
      expect(await store.get(ID('a'))).toMatchObject({ attemptsLeft: 5, active: true, passcode: { salt: 'aa', hash: 'bb' }, files: [] });

      const result = await store.update(ID('a'), async (link) => {
        link!.attemptsLeft -= 1;
        link!.active = false;
        link!.endedAt = 1234;
        return 'changed';
      });
      expect(result).toBe('changed');
      expect(await store.get(ID('a'))).toMatchObject({ attemptsLeft: 4, active: false, endedAt: 1234 });

      await expect(
        store.update(ID('a'), async (link, tx) => {
          link!.attemptsLeft = 99;
          await tx.putFile({ id: 'f0', contentType: 'x/y', bytes: 1 }, 'text');
          await tx.recordAccess({ recipient: 'r', at: 1, outcome: 'opened' }, 10);
          throw new Error('stop');
        }),
      ).rejects.toThrow('stop');
      expect(await store.get(ID('a'))).toMatchObject({ attemptsLeft: 4, files: [] });
      expect(await store.getFile(ID('a'), 'f0')).toBeNull();
      expect(await store.listAccess(ID('a'))).toEqual([]);

      // A link that is not there is handed over as null, so the caller says what that means.
      expect(await store.update(ID('z'), async (link) => link)).toBeNull();
    }));

  it('lets changes to one link take turns, however many arrive together', () =>
    using(async (store) => {
      await store.create(aLink(ID('a'), { attemptsLeft: 1000 }));
      await Promise.all(
        Array.from({ length: 40 }, () =>
          store.update(ID('a'), async (link) => {
            const before = link!.attemptsLeft;
            await new Promise((resolve) => setTimeout(resolve, 1));
            link!.attemptsLeft = before - 1;
          }),
        ),
      );
      expect(await second(store, ID('a'))).toBe(960);
    }));

  it('keeps files, reads them back, and removes them with the link\'s say-so', () =>
    using(async (store) => {
      await store.create(aLink(ID('a')));
      await store.update(ID('a'), async (link, tx) => {
        await tx.putFile({ id: 'f0', contentType: 'application/fhir+json', bytes: 5 }, 'first');
        await tx.putFile({ id: 'f1', contentType: 'application/jose', bytes: 6 }, 'second');
        expect(link!.files.map((file) => file.id)).toEqual(['f0', 'f1']);
      });
      expect(await store.getFile(ID('a'), 'f0')).toBe('first');
      expect(await store.getFile(ID('a'), 'f2')).toBeNull();
      expect(await store.getFile(ID('b'), 'f0')).toBeNull();
      expect((await store.get(ID('a')))!.files).toEqual([
        { id: 'f0', contentType: 'application/fhir+json', bytes: 5 },
        { id: 'f1', contentType: 'application/jose', bytes: 6 },
      ]);
      await store.update(ID('a'), async (link, tx) => {
        await tx.removeFiles();
        expect(link!.files).toEqual([]);
      });
      expect(await store.getFile(ID('a'), 'f0')).toBeNull();
      expect((await store.get(ID('a')))!.files).toEqual([]);
      expect(await store.get(ID('a'))).not.toBeNull();
    }));

  it('notes who asked, oldest first, and keeps only the latest few', () =>
    using(async (store) => {
      await store.create(aLink(ID('a')));
      await store.create(aLink(ID('b')));
      for (let index = 0; index < 5; index += 1) {
        await store.update(ID('a'), async (_link, tx) => {
          await tx.recordAccess({ recipient: `r${index}`, at: 100 + index, outcome: index % 2 ? 'wrong-passcode' : 'opened', ip: '203.0.113.7', userAgent: 'agent' }, 3);
        });
      }
      await store.update(ID('b'), async (_link, tx) => {
        await tx.recordAccess({ recipient: 'plain', at: 1, outcome: 'opened' }, 3);
      });
      expect((await store.listAccess(ID('a'))).map((entry) => entry.recipient)).toEqual(['r2', 'r3', 'r4']);
      expect((await store.listAccess(ID('a')))[1]).toEqual({ recipient: 'r3', at: 103, outcome: 'wrong-passcode', ip: '203.0.113.7', userAgent: 'agent' });
      expect(await store.listAccess(ID('b'))).toEqual([{ recipient: 'plain', at: 1, outcome: 'opened' }]);
    }));

  it('clears away what has ended, forgets it much later, and leaves the rest', () =>
    using(async (store) => {
      const now = Math.floor(Date.now() / 1000);
      const day = 86_400;
      const file = [{ id: 'f0', contentType: 'x/y', bytes: 10 }];
      const make = async (id: string, extra: Parameters<typeof aLink>[1]) => {
        await store.create(aLink(ID(id), { ...extra, files: [] }));
        if (extra?.files?.length) {
          await store.update(ID(id), async (_link, tx) => {
            for (const entry of extra.files!) await tx.putFile(entry, 'ciphertext');
          });
        }
      };
      await make('a', { expiresAt: now + day, files: file });
      await make('b', { expiresAt: now - 10, files: file });
      await make('c', { active: false, endedAt: now - 40 * day, expiresAt: now + day });
      await make('d', { active: false, endedAt: now - 5, files: file });
      await store.update(ID('b'), async (_link, tx) => tx.recordAccess({ recipient: 'r', at: now - 20, outcome: 'opened' }, 10));

      const counts = await store.sweep({ now, retainSeconds: 30 * day });
      expect(counts).toEqual({ links: 1, bytes: 10, emptied: 2, forgotten: 1 });
      expect(await store.getFile(ID('a'), 'f0')).toBe('ciphertext');
      expect(await store.getFile(ID('b'), 'f0')).toBeNull();
      expect(await store.getFile(ID('d'), 'f0')).toBeNull();
      expect(await store.get(ID('b'))).toMatchObject({ files: [], endedAt: now - 10 });
      expect(await store.listAccess(ID('b'))).toHaveLength(1);
      expect(await store.get(ID('c'))).toBeNull();

      expect(await store.sweep({ now, retainSeconds: 30 * day })).toEqual({ links: 1, bytes: 10, emptied: 0, forgotten: 0 });
      // Long enough after, the record of an ended link is forgotten, and who asked with it. The link
      // that expired a day from now has been over for exactly the 30 days kept, which is not more.
      expect(await store.sweep({ now: now + 31 * day, retainSeconds: 30 * day })).toMatchObject({ links: 0, forgotten: 2 });
      expect(await store.get(ID('b'))).toBeNull();
      expect(await store.listAccess(ID('b'))).toEqual([]);
      expect(await store.get(ID('a'))).not.toBeNull();
      expect(await store.sweep({ now: now + 32 * day, retainSeconds: 30 * day })).toMatchObject({ forgotten: 1 });
    }));

  it('makes a secret once, and gives the same one to everyone who asks', () =>
    using(async (store) => {
      const secrets = await Promise.all(Array.from({ length: 8 }, () => store.secret('ticket')));
      expect(new Set(secrets).size).toBe(1);
      expect(secrets[0]!.length).toBeGreaterThanOrEqual(43);
      expect(await store.secret('ticket')).toBe(secrets[0]);
      expect(await store.secret('other')).not.toBe(secrets[0]);
    }));

  it('says it can take a write', () => using((store) => store.ping()));
});

describe('sqlite store, as several processes would use it', () => {
  async function onOneFile(test: (open: (options?: { busyTimeoutSeconds?: number }) => Promise<Store>, file: string) => Promise<void>) {
    const sqlite = sqliteFile();
    try {
      await test((options) => sqlite.open(options), sqlite.file);
    } finally {
      await sqlite.done();
    }
  }

  it('does not lose the changes of one connection to another', () =>
    onOneFile(async (open) => {
      const first = await open();
      const others = await Promise.all([open(), open(), open()]);
      await first.create(aLink(ID('a'), { attemptsLeft: 1000 }));
      await Promise.all(
        [first, ...others].flatMap((store) =>
          Array.from({ length: 25 }, () =>
            store.update(ID('a'), async (link) => {
              const before = link!.attemptsLeft;
              await new Promise((resolve) => setTimeout(resolve, Math.random() * 3));
              link!.attemptsLeft = before - 1;
            }),
          ),
        ),
      );
      expect((await first.get(ID('a')))!.attemptsLeft).toBe(900);
    }));

  it('runs a change again, on the link as it is, when another connection got there first', () =>
    onOneFile(async (open) => {
      const a = await open();
      const b = await open();
      await a.create(aLink(ID('a'), { attemptsLeft: 1000 }));
      let release!: () => void;
      const gate = new Promise<void>((resolve) => (release = resolve));
      const seen: number[] = [];
      const slow = a.update(ID('a'), async (link) => {
        seen.push(link!.attemptsLeft);
        link!.attemptsLeft -= 1;
        if (seen.length === 1) await gate; // overtaken while it waits
      });
      await new Promise((resolve) => setTimeout(resolve, 50));
      await b.update(ID('a'), async (link) => void (link!.attemptsLeft -= 10));
      release();
      await slow;
      expect(seen).toEqual([1000, 990]); // the first run was thrown away, the second saw what the other kept
      expect((await a.get(ID('a')))!.attemptsLeft).toBe(989);
    }));

  // A sweep changes a link without a change of its own, so it has to be seen by one that is in progress:
  // what that change read is not what is there, and what it writes back must not undo the sweep.
  it.each([
    ['ends a link that has expired, which was not yet marked ended', { expiresAt: -10 }, false],
    ['empties the files of a link that was ended some time ago', { active: false, endedAt: -5 }, true],
  ] as const)('runs a change again when a sweep, in another connection, %s', (_what, extra, withFile) =>
    onOneFile(async (open) => {
      const a = await open();
      const b = await open();
      const now = Math.floor(Date.now() / 1000);
      const shifted = Object.fromEntries(Object.entries(extra).map(([key, value]) => [key, typeof value === 'number' ? now + value : value]));
      await a.create(aLink(ID('a'), shifted));
      if (withFile) await a.update(ID('a'), async (_link, tx) => tx.putFile({ id: 'f0', contentType: 'x/y', bytes: 4 }, 'text'));
      let release!: () => void;
      const gate = new Promise<void>((resolve) => (release = resolve));
      const seen: Array<{ files: number; endedAt: number | null }> = [];
      const slow = a.update(ID('a'), async (link) => {
        seen.push({ files: link!.files.length, endedAt: link!.endedAt });
        if (seen.length === 1) await gate;
      });
      await new Promise((resolve) => setTimeout(resolve, 50));
      await b.sweep({ now, retainSeconds: 3600 });
      release();
      await slow;
      expect(seen, 'the change was run again, on the link as the sweep left it').toHaveLength(2);
      expect(seen[1]).toEqual({ files: 0, endedAt: (shifted.endedAt as number | undefined) ?? (shifted.expiresAt as number) });
      expect((await a.get(ID('a')))!.endedAt, 'and its write-back did not undo the sweep').toBe(seen[1]!.endedAt);
      expect(await a.getFile(ID('a'), 'f0')).toBeNull();
    }));

  it('tells a caller to try again, when another process holds the file for too long, and opens while one is writing', () =>
    onOneFile(async (open, file) => {
      const store = await open({ busyTimeoutSeconds: 0.3 });
      await store.create(aLink(ID('a')));
      const { DatabaseSync } = await import('node:sqlite');
      const raw = new DatabaseSync(file);
      raw.exec('PRAGMA busy_timeout = 100');
      raw.exec('BEGIN IMMEDIATE'); // another process, in the middle of a write
      try {
        // A host that starts now does not wait to find the schema is there.
        const started = Date.now();
        const starting = await open();
        expect(Date.now() - started).toBeLessThan(2000);
        expect(await starting.get(ID('a'))).not.toBeNull(); // and what is committed can be read meanwhile
        // The wait for the write is the whole process's, which is why it is kept short.
        await expect(store.update(ID('a'), async (link) => void (link!.attemptsLeft -= 1))).rejects.toBeInstanceOf(StoreBusy);
      } finally {
        raw.exec('ROLLBACK');
        raw.close();
      }
      expect(await store.update(ID('a'), async (link) => link?.attemptsLeft)).toBe(5);
    }));

  it('keeps what it was given across a close and a reopen, and migrates once however many open at the same moment', async () => {
    const sqlite = sqliteFile();
    try {
      const opened = await Promise.all(Array.from({ length: 5 }, () => sqlite.open()));
      await opened[0]!.create(aLink(ID('a')));
      await opened[0]!.update(ID('a'), async (_link, tx) => tx.putFile({ id: 'f0', contentType: 'x/y', bytes: 2 }, 'hi'));
      await opened[0]!.secret('ticket');
      await Promise.all(opened.map((store) => store.close()));
      const again = await sqlite.open();
      expect(await again.getFile(ID('a'), 'f0')).toBe('hi');
      expect((await again.get(ID('a')))!.files).toHaveLength(1);
      const { DatabaseSync } = await import('node:sqlite');
      const raw = new DatabaseSync(sqlite.file);
      expect(raw.prepare('SELECT count(*) AS n FROM schema_migrations').get()).toEqual({ n: 1 });
      raw.close();
    } finally {
      await sqlite.done();
    }
  });

  it('makes a backup that is the whole database at one moment, while it is being written to, and will not overwrite one', () =>
    onOneFile(async (open, file) => {
      const store = await open();
      const other = await open();
      await store.create(aLink(ID('a')));
      await store.update(ID('a'), async (_link, tx) => {
        await tx.putFile({ id: 'f0', contentType: 'x/y', bytes: 6 }, 'secret');
        await tx.recordAccess({ recipient: 'r', at: 1, outcome: 'opened' }, 10);
      });
      await store.secret('ticket');
      // Writes go on in another connection while the backup is made.
      const writing = Promise.all(Array.from({ length: 20 }, () => other.update(ID('a'), async (link) => void (link!.attemptsLeft -= 0))));
      const to = `${file}.backup/copy.db`;
      await (store as import('../src/store/sqlite.js').SqliteStore).backup(to);
      await writing;

      const copy = await sqliteFile().open({ path: to, migrate: false });
      try {
        expect((await copy.get(ID('a')))?.files).toEqual([{ id: 'f0', contentType: 'x/y', bytes: 6 }]);
        expect(await copy.getFile(ID('a'), 'f0')).toBe('secret');
        expect((await copy.listAccess(ID('a'))).map((entry) => entry.recipient)).toEqual(['r']);
        expect(await copy.secret('ticket')).toBe(await store.secret('ticket'));
      } finally {
        await copy.close();
      }
      await expect((store as import('../src/store/sqlite.js').SqliteStore).backup(to)).rejects.toThrow(/exists: a backup is made to a new file/);
    }));

  it.skipIf(process.platform === 'win32')('keeps its files, and a backup, to the host alone', () =>
    onOneFile(async (open, file) => {
      const store = await open();
      await store.create(aLink(ID('a')));
      await store.update(ID('a'), async (link) => void (link!.attemptsLeft -= 1)); // so that the -wal file is there
      const mode = (name: string) => fs.statSync(name).mode & 0o777;
      expect(mode(file)).toBe(0o600);
      for (const beside of [`${file}-wal`, `${file}-shm`]) if (fs.existsSync(beside)) expect(mode(beside), beside).toBe(0o600);
      const to = `${file}.backup/copy.db`;
      await (store as import('../src/store/sqlite.js').SqliteStore).backup(to);
      expect(mode(to)).toBe(0o600);
      expect(mode(path.dirname(to))).toBe(0o700);
      await expect((store as import('../src/store/sqlite.js').SqliteStore).backup(`${file}.backup/copy.db`)).rejects.toThrow(/exists/);
      expect(fs.readdirSync(path.dirname(to))).toEqual(['copy.db']); // a refused backup leaves nothing of its own
    }));

  it('opens several stores at once and leaves process.emitWarning as it found it', async () => {
    const before = process.emitWarning;
    const sqlite = sqliteFile();
    try {
      await Promise.all(Array.from({ length: 6 }, () => sqlite.open()));
      expect(process.emitWarning).toBe(before);
    } finally {
      await sqlite.done();
    }
  });

  it('will not open a file that has no schema when it is told not to make one, and makes the directory it lives in', async () => {
    const sqlite = sqliteFile();
    try {
      await expect(sqlite.open({ migrate: false })).rejects.toThrow(/the database has no schema/);
      const nested = await sqlite.open({ path: `${sqlite.dir}/a/b/links.db` });
      await nested.create(aLink(ID('a')));
      expect(await nested.get(ID('a'))).not.toBeNull();
    } finally {
      await sqlite.done();
    }
  });
});

describe.skipIf(!databaseUrl)('postgres store, as several processes would use it', () => {
  it('does not lose the changes of one process to another', async () => {
    const schema = await schemaStore();
    try {
      const first = await schema.open();
      const others = await Promise.all([schema.open(), schema.open(), schema.open()]);
      await first.create(aLink(ID('a'), { attemptsLeft: 1000 }));
      await Promise.all(
        [first, ...others].flatMap((store) =>
          Array.from({ length: 25 }, () =>
            store.update(ID('a'), async (link) => {
              const before = link!.attemptsLeft;
              await new Promise((resolve) => setTimeout(resolve, Math.random() * 3));
              link!.attemptsLeft = before - 1;
            }),
          ),
        ),
      );
      expect((await first.get(ID('a')))!.attemptsLeft).toBe(900);
    } finally {
      await schema.done();
    }
  });

  it('migrates once however many start at the same moment, and again does nothing', async () => {
    const schema = await schemaStore({ migrate: true });
    try {
      const opened = await Promise.all(Array.from({ length: 5 }, () => schema.open()));
      const raw = schema.raw();
      const [row] = await raw<{ n: number }[]>`SELECT count(*)::int AS n FROM schema_migrations`;
      expect(row?.n).toBe(1);
      await raw.end({ timeout: 1 });
      await opened[0]!.create(aLink(ID('a')));
      await schema.open(); // opening it again leaves what is there
      expect(await opened[1]!.get(ID('a'))).not.toBeNull();
    } finally {
      await schema.done();
    }
  });

  it('tells a caller to try again, when a link is held by another change for too long', async () => {
    const schema = await schemaStore({ lockTimeoutSeconds: 1 });
    try {
      const store = await schema.open();
      await store.create(aLink(ID('a')));
      const raw = schema.raw();
      let release!: () => void;
      const holding = new Promise<void>((resolve) => (release = resolve));
      const held = raw.begin(async (tx) => {
        await tx`SELECT 1 FROM links WHERE id = ${ID('a')} FOR UPDATE`;
        await holding;
      });
      await new Promise((resolve) => setTimeout(resolve, 200));
      const started = Date.now();
      await expect(store.update(ID('a'), async (link) => link?.attemptsLeft)).rejects.toBeInstanceOf(StoreBusy);
      expect(Date.now() - started).toBeLessThan(5000);
      release();
      await held;
      await raw.end({ timeout: 1 });
      expect(await store.update(ID('a'), async (link) => link?.attemptsLeft)).toBe(5);
    } finally {
      await schema.done();
    }
  });

  it('leaves a sweep to one process at a time', async () => {
    const schema = await schemaStore();
    try {
      const store = await schema.open();
      const raw = schema.raw();
      let release!: () => void;
      const holding = new Promise<void>((resolve) => (release = resolve));
      const held = raw.begin(async (tx) => {
        await tx`SELECT pg_advisory_xact_lock(hashtext('refhir-link-host-sweep'))`;
        await holding;
      });
      await new Promise((resolve) => setTimeout(resolve, 200));
      expect(await store.sweep({ now: 1, retainSeconds: 1 })).toBeNull();
      release();
      await held;
      await raw.end({ timeout: 1 });
      expect(await store.sweep({ now: 1, retainSeconds: 1 })).toEqual({ links: 0, bytes: 0, emptied: 0, forgotten: 0 });
    } finally {
      await schema.done();
    }
  });

  it('says it cannot take a write when the database is read-only, and when it is not there', async () => {
    const { postgresStore } = await import('../src/store/postgres.js');
    // `default_transaction_read_only` is set on a whole database, and the other tests are using the one
    // they share while this one runs: this one has a database of its own.
    const scratch = await scratchDatabase();
    await scratch.create();
    const raw = scratch.connect();
    const opened: Store[] = [];
    try {
      const store = await postgresStore({ url: scratch.url });
      opened.push(store);
      await raw`ALTER DATABASE ${raw(scratch.name)} SET default_transaction_read_only = on`;
      const readOnly = await postgresStore({ url: scratch.url, migrate: false });
      opened.push(readOnly);
      await expect(readOnly.ping()).rejects.toThrow(/read-only/);
      await expect(store.ping()).resolves.toBeUndefined(); // an open connection is as it was made
    } finally {
      await Promise.allSettled(opened.map((store) => store.close()));
      await raw.end({ timeout: 1 });
      await scratch.done();
    }
    await expect(postgresStore({ url: 'postgres://nobody:nothing@127.0.0.1:1/none' })).rejects.toThrow();
  });
});

afterAll(() => undefined);

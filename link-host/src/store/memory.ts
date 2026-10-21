import { randomBytes } from 'node:crypto';

import type { AccessEntry, Link, LinkTx, Store, SweepCounts } from '../types.js';
import { endOf, inTurns } from './shared.js';

/** Links and files in memory, which a restart loses: for trying a host out, and for tests. */
export function memoryStore({ now = () => Date.now() }: { now?: () => number } = {}): Store {
  const links = new Map<string, Link>();
  const files = new Map<string, string>();
  const access = new Map<string, AccessEntry[]>();
  const secrets = new Map<string, string>();
  // Changes to one link take turns, which is what a lock on its row is in a database.
  const inTurn = inTurns();
  const dropFiles = (id: string) => {
    for (const key of [...files.keys()]) if (key.startsWith(`${id}/`)) files.delete(key);
  };

  const store: Store = {
    async create(link) {
      if (links.has(link.id)) throw new Error('that link exists');
      links.set(link.id, structuredClone(link));
    },
    async get(id) {
      const link = links.get(id);
      return link ? structuredClone(link) : null;
    },
    update(id, change) {
      return inTurn(id, async () => {
        const stored = links.get(id);
        const link = stored ? structuredClone(stored) : null;
        // What the change does through `tx` is kept only if it returns, as a transaction would.
        const putFiles: Array<[string, string]> = [];
        const entries: Array<[AccessEntry, number]> = [];
        let cleared = false;
        const tx: LinkTx = {
          async putFile(file, text) {
            link?.files.push(file);
            putFiles.push([file.id, text]);
          },
          async removeFiles() {
            if (link) link.files = [];
            putFiles.length = 0;
            cleared = true;
          },
          async recordAccess(entry, keep) {
            entries.push([entry, keep]);
          },
        };
        const result = await change(link, tx);
        if (link) {
          links.set(id, link);
          if (cleared) dropFiles(id);
          for (const [name, text] of putFiles) files.set(`${id}/${name}`, text);
          for (const [entry, keep] of entries) access.set(id, [...(access.get(id) ?? []), entry].slice(-keep));
        }
        return result;
      });
    },
    async getFile(id, name) {
      return files.get(`${id}/${name}`) ?? null;
    },
    async listAccess(id) {
      return structuredClone(access.get(id) ?? []);
    },
    async secret(name) {
      let value = secrets.get(name);
      if (!value) {
        value = randomBytes(32).toString('base64url');
        secrets.set(name, value);
      }
      return value;
    },
    async sweep({ now: moment, retainSeconds }) {
      const counts: SweepCounts = { links: 0, bytes: 0, emptied: 0, forgotten: 0 };
      for (const [id, link] of [...links]) {
        const ended = endOf(link, moment);
        if (ended === null) {
          counts.links += 1;
          counts.bytes += link.files.reduce((total, file) => total + file.bytes, 0);
        } else if (moment - ended > retainSeconds) {
          links.delete(id);
          access.delete(id);
          dropFiles(id);
          counts.forgotten += 1;
        } else if (link.files.length) {
          dropFiles(id);
          link.files = [];
          link.endedAt ??= ended;
          counts.emptied += 1;
        }
      }
      void now;
      return counts;
    },
    async ping() {},
    async close() {},
  };
  return store;
}

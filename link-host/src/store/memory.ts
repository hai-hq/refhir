import { randomBytes } from 'node:crypto';

import { NameTaken, type AccessEntry, type Link, type LinkTx, type Store, type StoredClient, type SweepCounts } from '../types.js';
import { endOf, inTurns, listed, newerFirst } from './shared.js';

/** Links and files in memory, which a restart loses: for trying a host out, and for tests. */
export function memoryStore({ now = () => Date.now() }: { now?: () => number } = {}): Store {
  const links = new Map<string, Link>();
  const files = new Map<string, string>();
  const access = new Map<string, AccessEntry[]>();
  const secrets = new Map<string, string>();
  const clients = new Map<string, StoredClient>();
  const nameTaken = (name: string, id: string) => [...clients.values()].some((client) => client.name === name && client.id !== id);
  // Changes to one link take turns, which is what a lock on its row is in a database.
  const inTurn = inTurns();
  const dropFiles = (id: string) => {
    for (const key of [...files.keys()]) if (key.startsWith(`${id}/`)) files.delete(key);
  };

  const store: Store = {
    async create(link) {
      if (links.has(link.id)) throw new Error('that link exists');
      // A link that does not say whose it is, is the client's that made it, as in a database.
      links.set(link.id, { ...structuredClone(link), clientId: link.clientId ?? link.createdBy });
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

    async listLinks(clientId, { limit, before }) {
      return [...links.values()]
        .filter((link) => link.clientId === clientId)
        .sort(newerFirst)
        .filter((link) => !before || newerFirst(before, link) < 0)
        .slice(0, limit)
        .map(listed);
    },
    async createClient(client) {
      if (nameTaken(client.name, client.id)) throw new NameTaken(client.name);
      clients.set(client.id, structuredClone(client));
    },
    async getClient(id) {
      const client = clients.get(id);
      return client ? structuredClone(client) : null;
    },
    async clientByToken(tokenHash) {
      const client = [...clients.values()].find((candidate) => candidate.tokenHash === tokenHash);
      return client ? structuredClone(client) : null;
    },
    async listClients() {
      return [...clients.values()].map((client) => structuredClone(client));
    },
    async updateClient(id, change) {
      const client = clients.get(id);
      if (!client) return null;
      if (change.name !== undefined && nameTaken(change.name, id)) throw new NameTaken(change.name);
      Object.assign(client, structuredClone(change));
      return structuredClone(client);
    },
    async deleteClient(id) {
      return clients.delete(id);
    },
  };
  return store;
}

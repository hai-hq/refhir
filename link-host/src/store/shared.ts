import type { Link, LinkListed } from '../types.js';

/** When a link stopped being servable, or `null` if it still can be. */
export function endOf(link: Pick<Link, 'active' | 'endedAt' | 'expiresAt'>, now: number): number | null {
  if (!link.active) return link.endedAt ?? now;
  if (link.expiresAt && link.expiresAt <= now) return link.expiresAt;
  return null;
}

/** Whether a link can be served. */
export const isLive = (link: Pick<Link, 'active' | 'endedAt' | 'expiresAt'> | null, now: number): link is Link =>
  link !== null && endOf(link, now) === null;

/**
 * Work for one key takes turns, which is what a lock on a row is in a database. A store whose
 * database cannot hold a link while a change runs uses it so that one process does not overtake
 * itself.
 */
export function inTurns(): <T>(key: string, work: () => Promise<T>) => Promise<T> {
  const turns = new Map<string, Promise<unknown>>();
  return (key, work) => {
    const run = (turns.get(key) ?? Promise.resolve()).then(work);
    const tail = run.catch(() => undefined);
    turns.set(key, tail);
    void tail.then(() => {
      if (turns.get(key) === tail) turns.delete(key);
    });
    return run;
  };
}

/** What a client is told of one of its links: nothing that opens it. */
export const listed = (link: Link): LinkListed => ({
  id: link.id,
  createdBy: link.createdBy,
  createdAt: link.createdAt,
  expiresAt: link.expiresAt,
  endedAt: link.endedAt,
  active: link.active,
  passcodeRequired: Boolean(link.passcode),
  files: link.files.length,
});

/** The order a client's links are listed in: newest first, and by id among those made in one second. */
export const newerFirst = (a: { createdAt: number; id: string }, b: { createdAt: number; id: string }): number =>
  b.createdAt - a.createdAt || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0);

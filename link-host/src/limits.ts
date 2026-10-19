// How many requests a caller may make in a window of time.
//
// Counts are kept in this process, so with several processes behind one address each allows its own
// share. The count of keys is bounded: a caller who makes a new key for every request (a new address
// each time) makes the oldest ones forgotten and cannot make the memory grow.

export interface Verdict {
  ok: boolean;
  /** Seconds until the window is over. */
  retryAfter: number;
}

export interface RateLimiter {
  /** Counts a request. */
  hit(key: string): Verdict;
  /** Says whether a caller is over the limit, without counting: for a limit on failures that only
   * counts those. */
  blocked(key: string): Verdict;
  readonly size: number;
}

/** A fixed window for each key. A `limit` of 0 allows everything. */
export function rateLimiter({
  limit,
  windowSeconds = 60,
  now = () => Date.now(),
  maxKeys = 10_000,
}: {
  limit: number;
  windowSeconds?: number;
  now?: () => number;
  maxKeys?: number;
}): RateLimiter {
  const windows = new Map<string, { count: number; resetAt: number }>();
  const windowMs = windowSeconds * 1000;

  function current(key: string) {
    const moment = now();
    const found = windows.get(key);
    if (found && found.resetAt > moment) return found;
    if (found) windows.delete(key);
    if (windows.size >= maxKeys) {
      for (const [other, entry] of windows) if (entry.resetAt <= moment) windows.delete(other);
      // Still full: the oldest windows, which are the ones nearest to being over, go first.
      for (const other of windows.keys()) {
        if (windows.size < maxKeys) break;
        windows.delete(other);
      }
    }
    const fresh = { count: 0, resetAt: moment + windowMs };
    windows.set(key, fresh);
    return fresh;
  }
  const retryAfter = (entry: { resetAt: number }) => Math.max(1, Math.ceil((entry.resetAt - now()) / 1000));

  return {
    hit(key) {
      if (!limit) return { ok: true, retryAfter: 0 };
      const entry = current(key);
      entry.count += 1;
      return { ok: entry.count <= limit, retryAfter: retryAfter(entry) };
    },
    blocked(key) {
      if (!limit) return { ok: true, retryAfter: 0 };
      const entry = windows.get(key);
      if (!entry || entry.resetAt <= now()) return { ok: true, retryAfter: 0 };
      return { ok: entry.count < limit, retryAfter: retryAfter(entry) };
    },
    get size() {
      return windows.size;
    },
  };
}

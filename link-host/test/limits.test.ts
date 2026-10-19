import { describe, expect, it } from 'vitest';

import { rateLimiter } from '../src/limits.js';

function clocked() {
  let clock = 1_000_000;
  return { now: () => clock, advance: (seconds: number) => void (clock += seconds * 1000) };
}

describe('a rate limiter', () => {
  it('gives a caller as many requests as the limit allows in a window, and then says when to return', () => {
    const time = clocked();
    const limiter = rateLimiter({ limit: 3, windowSeconds: 60, now: time.now });
    expect([1, 2, 3].map(() => limiter.hit('a').ok)).toEqual([true, true, true]);
    const refused = limiter.hit('a');
    expect(refused.ok).toBe(false);
    expect(refused.retryAfter).toBe(60);
    time.advance(45);
    expect(limiter.hit('a').retryAfter).toBe(15); // the wait shrinks as the window runs out
    expect(limiter.hit('b').ok).toBe(true); // another caller has a count of its own
    time.advance(16);
    expect(limiter.hit('a').ok).toBe(true); // and a new window starts clean
  });

  it('allows everything with a limit of 0', () => {
    const limiter = rateLimiter({ limit: 0 });
    for (let index = 0; index < 1000; index += 1) expect(limiter.hit('a').ok).toBe(true);
    expect(limiter.blocked('a').ok).toBe(true);
    expect(limiter.size).toBe(0); // and keeps nothing
  });

  it('counts only the failures it is told of, for a limit on failures, and blocks when they are used up', () => {
    const time = clocked();
    const limiter = rateLimiter({ limit: 3, windowSeconds: 300, now: time.now });
    for (let index = 0; index < 10; index += 1) expect(limiter.blocked('a').ok).toBe(true); // asking costs nothing
    limiter.hit('a');
    limiter.hit('a');
    expect(limiter.blocked('a').ok).toBe(true); // two of three
    limiter.hit('a');
    const barred = limiter.blocked('a');
    expect(barred.ok).toBe(false); // three of three
    expect(barred.retryAfter).toBeGreaterThan(0);
    expect(barred.retryAfter).toBeLessThanOrEqual(300);
    time.advance(301);
    expect(limiter.blocked('a').ok).toBe(true);
  });

  it('cannot be made to use more memory by callers who make a new key for every request', () => {
    const time = clocked();
    const limiter = rateLimiter({ limit: 5, windowSeconds: 60, now: time.now, maxKeys: 100 });
    for (let index = 0; index < 10_000; index += 1) limiter.hit(`address-${index}`);
    expect(limiter.size).toBeLessThanOrEqual(100);
    // Windows that are over are dropped before a live one is.
    const fresh = rateLimiter({ limit: 5, windowSeconds: 60, now: time.now, maxKeys: 3 });
    fresh.hit('old-1');
    fresh.hit('old-2');
    time.advance(61);
    fresh.hit('live-1');
    fresh.hit('live-2');
    fresh.hit('live-3');
    expect(fresh.blocked('live-1').ok).toBe(true);
    fresh.hit('live-1');
    expect(fresh.size).toBe(3);
  });
});

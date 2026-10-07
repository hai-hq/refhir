import { errClosed } from "./errors.js";
import type { JsonValue } from "./types.js";

type Waiter = {
  after: number;
  fulfill: (value: JsonValue) => void;
  fail: (err: Error) => void;
};

export class Watch {
  private readonly waiters: Waiter[] = [];
  private current: JsonValue | null = null;
  private seq = 0;
  private seen = 0;
  private error: Error | null = null;
  private closed = false;

  /** @internal */
  id = 0;

  /** @internal */
  constructor(private readonly remove: () => Promise<void>) {}

  latest(): JsonValue | null {
    return this.current;
  }

  async next(): Promise<JsonValue> {
    if (this.seq > this.seen) {
      const value = this.current!;
      this.seen = this.seq;
      return value;
    }
    if (this.error) throw this.error;
    if (this.closed) throw errClosed;
    return new Promise<JsonValue>((resolve, reject) => {
      this.waiters.push({ after: this.seen, fulfill: resolve, fail: reject });
    });
  }

  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    if (!this.error) this.error = errClosed;
    this.failWaiters(this.error);
    await this.remove();
  }

  /** @internal */
  deliver(value: JsonValue): void {
    if (this.closed) return;
    this.current = value;
    this.seq += 1;
    this.wakeWaiters();
  }

  /** @internal */
  fail(err: Error): void {
    if (this.closed) return;
    if (!this.error) this.error = err;
    this.failWaiters(err);
  }

  private wakeWaiters(): void {
    const pending = this.waiters.splice(0);
    for (const waiter of pending) {
      if (this.seq > waiter.after) {
        this.seen = this.seq;
        waiter.fulfill(this.current!);
      } else {
        this.waiters.push(waiter);
      }
    }
  }

  private failWaiters(err: Error): void {
    const pending = this.waiters.splice(0);
    for (const waiter of pending) {
      waiter.fail(err);
    }
  }

  /** @internal */
  async waitForFirst(): Promise<JsonValue> {
    if (this.seq > 0) {
      this.seen = this.seq;
      return this.current!;
    }
    return this.next();
  }
}

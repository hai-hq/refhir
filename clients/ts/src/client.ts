import {
  errClosed,
  errDisconnected,
  errNotSent,
  OutcomeError,
  QueryError,
  UpgradeError,
} from "./errors.js";
import { modifyQuerySet, mutationMessage, type QueryModification, type StateModification } from "./protocol.js";
import type { SocketConnection } from "./socket.js";
import { SOCKET_OPEN } from "./socket.js";
import type { Args, JsonValue } from "./types.js";
import * as helpers from "./helpers.js";
import * as rest from "./rest.js";
import { Watch } from "./watch.js";

/**
 * Gives the bearer token for the next `/sync` upgrade or REST call. The client asks it each time it
 * needs one, so it may return a token it has renewed. Return `undefined` or an empty string to
 * send no `Authorization`.
 */
export type TokenSource = () => Promise<string | undefined> | string | undefined;

export type DialOptions = {
  /**
   * The bearer token, or a function that gives one. A string is always the same token, and a
   * session that outlives it ends when the server stops accepting it. A function is called for the
   * `/sync` upgrade on the first dial and on every redial, and for REST calls: use it for tokens
   * that expire, such as a gateway's five-minute OAuth client-credentials tokens.
   *
   * When the server refuses an upgrade with HTTP 401, the client asks the function once more and
   * dials again before it treats the refusal as final. A second 401, or a function that returns the
   * very token just refused, ends the session; so does 403. An error from the function is not
   * final: a redial tries again after the usual backoff, and `dial` rejects with it.
   *
   * In a browser a refused upgrade cannot be told from any other failure, so every failure is
   * retried, asking the function each time.
   */
  token?: string | TokenSource;
  /** Default: browser when `window` exists, else Node (`ws` + Authorization header). */
  runtime?: "browser" | "node";
  /**
   * Echo the highest commit timestamp seen as `X-Refhir-Min-Ts` on REST calls so
   * a replica never serves a read older than this client's own writes.
   * Default: true. `lastCommitTs` tracks either way.
   *
   * Timestamps come from `X-Refhir-Commit-Ts` on REST responses and `ts` on
   * `/sync` mutation responses. In browsers `fetch` can only read the REST
   * header if the server lists it in `Access-Control-Expose-Headers`; mutations
   * over `/sync` always count.
   */
  readYourWrites?: boolean;
};

/** Options for one `mutate` or `watch` call. */
export type CallOptions = {
  /**
   * Gives up on the call. A call whose signal has aborted is not sent, and one waiting for its
   * answer rejects with the signal's reason. A mutation that was already sent may still be applied.
   */
  signal?: AbortSignal;
};

/** @internal The source `dial` hands to the client for a `token` option. */
export function tokenSource(token: string | TokenSource | undefined): TokenSource | undefined {
  if (typeof token === "function") return token;
  if (token === undefined) return undefined;
  return () => token;
}

/** Settle with `promise`, or reject with the signal's reason when it aborts first. */
function abortable<T>(promise: Promise<T>, signal: AbortSignal | undefined): Promise<T> {
  if (!signal) return promise;
  return new Promise<T>((resolve, reject) => {
    const onAbort = () => reject(signal.reason);
    if (signal.aborted) return onAbort();
    signal.addEventListener("abort", onAbort, { once: true });
    promise.then(resolve, reject).finally(() => signal.removeEventListener("abort", onAbort));
  });
}

type Subscription = {
  id: number;
  path: string;
  args: JsonValue;
  watch: Watch;
  sent: boolean;
};

/**
 * A change to the query set that is waiting for its answer: the Transition that holds one item for
 * each modification in it, whichever version it is at.
 */
type Flight = {
  want: Map<number, string>;
};

type PendingMutation = {
  resolve: (value: JsonValue) => void;
  reject: (err: Error) => void;
};

export class Client {
  private readonly httpBase: string;
  private readonly wsURL: string;
  private readonly tokens: TokenSource | undefined;
  private readonly readYourWrites: boolean;
  private lastTs = 0n;

  private socket: SocketConnection | null = null;
  private readonly openSocket: (
    wsURL: string,
    token: string | undefined,
  ) => Promise<SocketConnection>;
  private gen = 0;
  private version = 0;
  private nextQuery = 1;
  private nextReq = 1;
  private closing = false;
  /** Why the session ended, when the server ended it (an upgrade it refused for good). */
  private terminal: Error | null = null;
  private wakeReconnect: (() => void) | null = null;

  private readonly subs = new Map<number, Subscription>();
  private queue: QueryModification[] = [];
  private flight: Flight | null = null;
  private readonly pending = new Map<number, PendingMutation>();

  private cmdTail: Promise<void> = Promise.resolve();

  private constructor(
    httpBase: string,
    wsURL: string,
    tokens: TokenSource | undefined,
    openSocket: (wsURL: string, token: string | undefined) => Promise<SocketConnection>,
    readYourWrites: boolean,
  ) {
    this.httpBase = httpBase;
    this.wsURL = wsURL;
    this.tokens = tokens;
    this.openSocket = openSocket;
    this.readYourWrites = readYourWrites;
  }

  /** @internal Used by `dial()` in dial.ts */
  static async connect(
    httpBase: string,
    wsURL: string,
    tokens: TokenSource | undefined,
    openSocket: (wsURL: string, token: string | undefined) => Promise<SocketConnection>,
    options: { readYourWrites?: boolean } = {},
  ): Promise<Client> {
    const client = new Client(
      httpBase,
      wsURL,
      tokens,
      openSocket,
      options.readYourWrites ?? true,
    );
    await client.connectSocket();
    return client;
  }

  async close(): Promise<void> {
    this.closing = true;
    this.wakeReconnect?.();
    if (this.socket) {
      this.socket.close();
      this.socket = null;
    }
    this.failMutations(errClosed);
    for (const sub of this.subs.values()) {
      sub.watch.fail(errClosed);
    }
    this.subs.clear();
  }

  /**
   * Whether the session has a socket. It is false while the client dials again after the
   * connection dropped, and `mutate` rejects with `NotSentError` until it is true.
   */
  get connected(): boolean {
    return this.socket !== null && !this.closing;
  }

  /**
   * Run a write operation and wait for its `MutationResponse`. A failed mutation rejects with
   * `OutcomeError`. Mutations are not retried after a disconnect.
   *
   * Two errors say how far a mutation got when the connection was not there. `NotSentError`:
   * nothing was written, because the client was dialling again (or `signal` had aborted); try again
   * once `connected` is true. `DisconnectedError`: it was sent and the socket dropped before the
   * answer, so it may have been applied.
   */
  async mutate(path: string, args: Args, options: CallOptions = {}): Promise<JsonValue> {
    const { signal } = options;
    if (signal?.aborted) throw errNotSent;
    const sent = this.enqueue(async () => {
      // The caller may have given up while this waited its turn.
      if (signal?.aborted) throw errNotSent;
      if (this.terminal) throw this.terminal;
      if (!this.socket) throw errNotSent;
      const id = this.nextReq++;
      const result = new Promise<JsonValue>((resolve, reject) => {
        this.pending.set(id, { resolve, reject });
      });
      this.send(mutationMessage(id, path, args));
      return result;
    });
    return abortable(sent, signal);
  }

  /**
   * Subscribe to a read-only operation and wait for its first result. A watch whose `signal`
   * aborts first is dropped without being sent, or removed if it was.
   */
  async watch(path: string, args: Args = {}, options: CallOptions = {}): Promise<Watch> {
    const { signal } = options;
    signal?.throwIfAborted();
    const watch = new Watch(() =>
      this.enqueue(async () => {
        await this.removeWatch(watch);
      }),
    );
    await this.enqueue(async () => {
      signal?.throwIfAborted();
      if (this.terminal) throw this.terminal;
      this.addWatch(path, args, watch);
    });
    try {
      return await abortable(
        watch.waitForFirst().then(() => watch),
        signal,
      );
    } catch (err) {
      await watch.close();
      throw err;
    }
  }

  private async bearer(): Promise<string | undefined> {
    return this.tokens?.();
  }

  /**
   * Open the socket with the token the source gives. On a 401 it asks the source once more and
   * dials again, unless the source gives back the very token that was refused, which would only be
   * refused again.
   */
  private async openWithToken(): Promise<SocketConnection> {
    const token = await this.bearer();
    try {
      return await this.openSocket(this.wsURL, token);
    } catch (err) {
      if (!(err instanceof UpgradeError) || err.status !== 401) throw err;
      const fresh = await this.bearer();
      if (fresh === token) throw err;
      return this.openSocket(this.wsURL, fresh);
    }
  }

  private async connectSocket(): Promise<void> {
    const socket = await this.openWithToken();
    if (this.closing) {
      // close() ran while the socket was opening; do not leak it.
      socket.close();
      return;
    }
    this.gen += 1;
    const gen = this.gen;
    socket.onMessage((text) => {
      if (gen !== this.gen) return;
      this.onMessage(text);
    });
    socket.onClose(() => {
      if (gen !== this.gen || this.closing) return;
      void this.onDown();
    });
    this.socket = socket;
  }

  private async onDown(): Promise<void> {
    if (this.closing) return;
    this.gen += 1;
    this.failMutations(errDisconnected);
    this.flight = null;
    this.queue = [];
    this.socket = null;
    await this.reconnect();
    if (this.closing) return;
    this.version = 0;
    // Watches added while offline are already in `subs`; resubscribe re-adds all of them.
    this.queue = [];
    this.resubscribe();
  }

  /** Dial again until it works or the client closes: 50ms doubling to 1s, with jitter. */
  private async reconnect(): Promise<void> {
    let backoff = RECONNECT_MIN_MS;
    while (!this.closing) {
      let retryAfter = 0;
      try {
        await this.connectSocket();
        return;
      } catch (err) {
        if (err instanceof UpgradeError) {
          // A token the server does not accept will not be accepted by asking again.
          if (err.status === 401 || err.status === 403) {
            this.end(err);
            return;
          }
          retryAfter = err.retryAfterMs;
        }
        // fall through to the delay
      }
      const delay = Math.max(backoff / 2 + Math.random() * (backoff / 2), retryAfter);
      await new Promise<void>((resolve) => {
        const timer = setTimeout(resolve, delay);
        this.wakeReconnect = () => {
          clearTimeout(timer);
          resolve();
        };
      });
      this.wakeReconnect = null;
      backoff = Math.min(backoff * 2, RECONNECT_MAX_MS);
    }
  }

  /** The server refused the session for good: end it, and say why to everything waiting on it. */
  private end(err: Error): void {
    this.terminal = err;
    this.closing = true;
    this.failMutations(err);
    for (const sub of this.subs.values()) {
      sub.watch.fail(err);
    }
    this.subs.clear();
  }

  private resubscribe(): void {
    if (this.subs.size === 0) return;
    const ids = [...this.subs.keys()].sort((a, b) => a - b);
    for (const id of ids) {
      const sub = this.subs.get(id)!;
      sub.sent = true;
      this.queue.push({
        type: "Add",
        queryId: id,
        udfPath: sub.path,
        args: sub.args,
      });
    }
    this.flush();
  }

  private addWatch(path: string, args: Args, watch: Watch): void {
    const id = this.nextQuery++;
    watch.id = id;
    const argsJson = args as JsonValue;
    this.subs.set(id, { id, path, args: argsJson, watch, sent: false });
    this.queue.push({ type: "Add", queryId: id, udfPath: path, args: argsJson });
    this.flush();
  }

  private async removeWatch(watch: Watch): Promise<void> {
    let id = watch.id;
    const sub = this.subs.get(id);
    if (!sub) return;
    this.subs.delete(id);
    if (!sub.sent) {
      this.queue = this.queue.filter(
        (m) => !(m.type === "Add" && m.queryId === id),
      );
      return;
    }
    this.queue.push({ type: "Remove", queryId: id });
    this.flush();
  }

  private flush(): void {
    if (this.flight || !this.socket || this.queue.length === 0) return;
    const mods = this.queue.splice(0);
    const want = new Map<number, string>();
    for (const mod of mods) {
      want.set(mod.queryId, mod.type);
      if (mod.type === "Add") {
        const sub = this.subs.get(mod.queryId);
        if (sub) sub.sent = true;
      }
    }
    const base = this.version;
    this.send(modifyQuerySet(base, base + 1, mods));
    this.flight = { want };
  }

  private onMessage(text: string): void {
    let env: { type?: string };
    try {
      env = JSON.parse(text) as { type?: string };
    } catch {
      void this.dropSocket();
      return;
    }
    switch (env.type) {
      case "Transition":
        this.onTransition(JSON.parse(text) as {
          startVersion: number;
          endVersion: number;
          modifications: StateModification[];
        });
        break;
      case "MutationResponse": {
        const msg = JSON.parse(text) as {
          requestId: number;
          success: boolean;
          result: JsonValue;
          ts?: string | number | null;
        };
        this.onMutation(msg);
        break;
      }
      case "FatalError":
        void this.dropSocket();
        break;
      default:
        void this.dropSocket();
    }
  }

  private onTransition(msg: {
    startVersion: number;
    endVersion: number;
    modifications: StateModification[];
  }): void {
    if (msg.startVersion !== this.version || msg.endVersion <= msg.startVersion) {
      void this.dropSocket();
      return;
    }
    this.version = msg.endVersion;
    for (const mod of msg.modifications) {
      this.apply(mod);
    }
    // A push can land between the change being sent and its answer, so the answer is not at the
    // version the change was based on. It is found by what it holds: one item for each
    // modification. The server answers the change before it pushes anything for what it added.
    if (this.flight && flightMet(this.flight, msg.modifications)) {
      this.flight = null;
      this.flush();
    }
  }

  private apply(mod: StateModification): void {
    switch (mod.type) {
      case "QueryUpdated": {
        const sub = this.subs.get(mod.queryId);
        if (sub) sub.watch.deliver(mod.value);
        break;
      }
      case "QueryFailed": {
        const sub = this.subs.get(mod.queryId);
        if (!sub) break;
        this.subs.delete(mod.queryId);
        sub.watch.fail(new QueryError(sub.path, mod.error, mod.code ?? ""));
        break;
      }
      case "QueryRemoved":
        break;
    }
  }

  private onMutation(msg: {
    requestId: number;
    success: boolean;
    result: JsonValue;
    ts?: string | number | null;
  }): void {
    const pending = this.pending.get(msg.requestId);
    if (!pending) return;
    this.pending.delete(msg.requestId);
    if (msg.success) {
      // Record the timestamp before waking the caller so lastCommitTs and the
      // next REST request already reflect this write.
      this.observeTs(msg.ts);
      pending.resolve(msg.result);
      return;
    }
    pending.reject(new OutcomeError(msg.result));
  }

  private failMutations(err: Error): void {
    for (const [id, pending] of this.pending) {
      pending.reject(err);
      this.pending.delete(id);
    }
  }

  private send(value: JsonValue): void {
    if (!this.socket || this.socket.readyState !== SOCKET_OPEN) {
      void this.dropSocket();
      return;
    }
    this.socket.send(JSON.stringify(value));
  }

  private async dropSocket(): Promise<void> {
    if (!this.socket) return;
    this.socket.removeListeners();
    this.socket.terminate();
    this.socket = null;
    await this.onDown();
  }

  private enqueue<T>(fn: () => Promise<T>): Promise<T> {
    const run = this.cmdTail.then(fn);
    this.cmdTail = run.then(
      () => undefined,
      () => undefined,
    );
    return run;
  }

  metadata(): Promise<JsonValue> {
    return rest.metadata(this);
  }

  doRequest(method: string, path: string, body?: Uint8Array): Promise<Uint8Array> {
    return rest.doRequest(this, method, path, body);
  }

  /** Like `doRequest`, but also returns the status and response headers. */
  requestWithHeaders(
    method: string,
    path: string,
    body?: Uint8Array,
  ): Promise<rest.RestResponse> {
    return rest.requestWithHeaders(this, method, path, body);
  }

  search(
    resourceType: string,
    params: Record<string, string | string[]>,
  ): Promise<Watch> {
    return helpers.search(this, resourceType, params);
  }

  read(resourceType: string, id: string): Promise<Watch> {
    return helpers.read(this, resourceType, id);
  }

  query(path: string, args: Args = {}): Promise<JsonValue> {
    return helpers.query(this, path, args);
  }

  create(resourceType: string, resource: JsonValue): Promise<JsonValue> {
    return helpers.create(this, resourceType, resource);
  }

  update(
    resourceType: string,
    id: string,
    resource: JsonValue,
    ifMatch?: string,
  ): Promise<JsonValue> {
    return helpers.update(this, resourceType, id, resource, ifMatch);
  }

  delete(resourceType: string, id: string): Promise<JsonValue> {
    return helpers.deleteResource(this, resourceType, id);
  }

  transaction(bundle: JsonValue): Promise<JsonValue> {
    return helpers.transaction(this, bundle);
  }

  /**
   * Highest commit timestamp this client has seen (`0n` until the server
   * reports one). Raised by `X-Refhir-Commit-Ts` on REST responses and `ts` on
   * successful `/sync` mutations; never lowered.
   */
  get lastCommitTs(): bigint {
    return this.lastTs;
  }

  /** @internal Value for `X-Refhir-Min-Ts`, or undefined when nothing should be sent. */
  minTs(): bigint | undefined {
    return this.readYourWrites && this.lastTs > 0n ? this.lastTs : undefined;
  }

  /** @internal Raise `lastCommitTs` from a header or wire value; malformed values are ignored. */
  observeTs(raw: string | number | bigint | null | undefined): void {
    const ts = parseTs(raw);
    if (ts !== undefined && ts > this.lastTs) this.lastTs = ts;
  }

  /** @internal */
  httpOrigin(): string {
    return this.httpBase;
  }

  /** @internal The token to send now, from the source. */
  async authToken(): Promise<string | undefined> {
    return this.bearer();
  }
}

/** Parse a u64 timestamp from a decimal string (or a safe integer). Anything else is undefined. */
function parseTs(raw: string | number | bigint | null | undefined): bigint | undefined {
  if (typeof raw === "bigint") return raw >= 0n ? raw : undefined;
  if (typeof raw === "number") return Number.isSafeInteger(raw) && raw >= 0 ? BigInt(raw) : undefined;
  if (typeof raw !== "string") return undefined;
  const text = raw.trim();
  if (!/^\d+$/.test(text)) return undefined;
  const ts = BigInt(text);
  return ts <= U64_MAX ? ts : undefined;
}

const RECONNECT_MIN_MS = 50;
const RECONNECT_MAX_MS = 1000;

const U64_MAX = (1n << 64n) - 1n;

function flightMet(flight: Flight, mods: StateModification[]): boolean {
  const seen = new Map<number, string>();
  for (const mod of mods) {
    if (mod.type === "QueryUpdated" || mod.type === "QueryFailed") {
      seen.set(mod.queryId, "Add");
    } else if (mod.type === "QueryRemoved") {
      seen.set(mod.queryId, "Remove");
    }
  }
  for (const [id, kind] of flight.want) {
    if (seen.get(id) !== kind) return false;
  }
  return true;
}

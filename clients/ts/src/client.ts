import { errClosed, errDisconnected, OutcomeError } from "./errors.js";
import { modifyQuerySet, mutationMessage, type QueryModification, type StateModification } from "./protocol.js";
import type { SocketConnection } from "./socket.js";
import { SOCKET_OPEN } from "./socket.js";
import type { Args, JsonValue } from "./types.js";
import * as helpers from "./helpers.js";
import * as rest from "./rest.js";
import { Watch } from "./watch.js";

export type DialOptions = {
  token?: string;
  /** Default: browser when `window` exists, else Node (`ws` + Authorization header). */
  runtime?: "browser" | "node";
};

type Subscription = {
  id: number;
  path: string;
  args: JsonValue;
  watch: Watch;
  sent: boolean;
};

type Flight = {
  base: number;
  want: Map<number, string>;
};

type PendingMutation = {
  resolve: (value: JsonValue) => void;
  reject: (err: Error) => void;
};

export class Client {
  private readonly httpBase: string;
  private readonly wsURL: string;
  private readonly token: string | undefined;

  private socket: SocketConnection | null = null;
  private readonly openSocket: (wsURL: string) => Promise<SocketConnection>;
  private gen = 0;
  private version = 0;
  private nextQuery = 1;
  private nextReq = 1;
  private closing = false;

  private readonly subs = new Map<number, Subscription>();
  private queue: QueryModification[] = [];
  private flight: Flight | null = null;
  private readonly pending = new Map<number, PendingMutation>();

  private cmdTail: Promise<void> = Promise.resolve();

  private constructor(
    httpBase: string,
    wsURL: string,
    token: string | undefined,
    openSocket: (wsURL: string) => Promise<SocketConnection>,
  ) {
    this.httpBase = httpBase;
    this.wsURL = wsURL;
    this.token = token;
    this.openSocket = openSocket;
  }

  /** @internal Used by `dial()` in dial.ts */
  static async connect(
    httpBase: string,
    wsURL: string,
    token: string | undefined,
    openSocket: (wsURL: string) => Promise<SocketConnection>,
  ): Promise<Client> {
    const client = new Client(httpBase, wsURL, token, openSocket);
    await client.connectSocket();
    return client;
  }

  async close(): Promise<void> {
    this.closing = true;
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

  async mutate(path: string, args: Args): Promise<JsonValue> {
    return this.enqueue(async () => {
      if (!this.socket) throw errDisconnected;
      const id = this.nextReq++;
      const result = new Promise<JsonValue>((resolve, reject) => {
        this.pending.set(id, { resolve, reject });
      });
      this.send(mutationMessage(id, path, args));
      return result;
    });
  }

  async watch(path: string, args: Args = {}): Promise<Watch> {
    const watch = new Watch(() =>
      this.enqueue(async () => {
        await this.removeWatch(watch);
      }),
    );
    await this.enqueue(async () => {
      this.addWatch(path, args, watch);
    });
    try {
      return await watch.waitForFirst().then(() => watch);
    } catch (err) {
      await watch.close();
      throw err;
    }
  }

  private async connectSocket(): Promise<void> {
    const socket = await this.openSocket(this.wsURL);
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
    await this.connectSocket();
    this.version = 0;
    this.resubscribe();
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
    this.flight = { base, want };
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
    const start = msg.startVersion;
    this.version = msg.endVersion;
    for (const mod of msg.modifications) {
      this.apply(mod);
    }
    if (this.flight && start === this.flight.base && flightMet(this.flight, msg.modifications)) {
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
        sub.watch.fail(new Error(`refhir: ${sub.path}: ${mod.error}`));
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
  }): void {
    const pending = this.pending.get(msg.requestId);
    if (!pending) return;
    this.pending.delete(msg.requestId);
    if (msg.success) pending.resolve(msg.result);
    else pending.reject(new OutcomeError(msg.result));
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

  search(resourceType: string, params: Record<string, string>): Promise<Watch> {
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

  /** @internal */
  httpOrigin(): string {
    return this.httpBase;
  }

  /** @internal */
  authToken(): string | undefined {
    return this.token;
  }
}

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

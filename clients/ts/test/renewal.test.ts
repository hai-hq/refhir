import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import type { Duplex } from "node:stream";

import { afterEach, describe, expect, it } from "vitest";
import { WebSocketServer, type WebSocket } from "ws";

import {
  DisconnectedError,
  dial,
  NotSentError,
  QueryError,
  UpgradeError,
  type Client,
} from "../src/index.js";

type Conn = { version: number; live: Set<number> };

/**
 * A /sync server over one number. A live query answers with it, a Mutation adds one to it and tells
 * every socket that has a query, and a test can end every socket at will. It is the smallest server
 * on which "the watch sees a write made after the redial" can be asserted.
 */
class CounterSync {
  value = 0;
  readonly adds: number[] = [];
  readonly tokens: string[] = [];
  readonly restTokens: (string | undefined)[] = [];
  mutations = 0;
  /** True once a 401 has been answered. */
  refused = false;
  /** Whether an upgrade with this token is let in. */
  authorize: (token: string) => boolean = () => true;
  /** The status to turn upgrades away with, or 0 to let them in. */
  status = 0;
  /** What a Mutation is answered with, for a test that wants a server that does not. */
  mute = false;

  url = "";
  private readonly http: Server;
  private readonly sockets = new WebSocketServer({ noServer: true });
  private readonly conns = new Map<WebSocket, Conn>();

  constructor() {
    this.http = createServer((req, res) => {
      this.restTokens.push(req.headers.authorization);
      res.setHeader("content-type", "application/fhir+json");
      res.end("{}");
    });
    this.http.on("upgrade", (req, socket: Duplex, head) => {
      const token = (req.headers.authorization ?? "").replace(/^Bearer /, "");
      this.tokens.push(token);
      const refuse = (status: number) => {
        if (status === 401) this.refused = true;
        socket.write(`HTTP/1.1 ${status} Refused\r\nContent-Length: 0\r\nConnection: close\r\n\r\n`);
        socket.destroy();
      };
      if (this.status !== 0) return refuse(this.status);
      if (!this.authorize(token)) return refuse(401);
      this.sockets.handleUpgrade(req, socket, head, (ws) => this.accept(ws));
    });
  }

  async start(): Promise<void> {
    await new Promise<void>((resolve) => this.http.listen(0, "127.0.0.1", resolve));
    this.url = `http://127.0.0.1:${(this.http.address() as AddressInfo).port}`;
  }

  async stop(): Promise<void> {
    for (const ws of this.sockets.clients) ws.terminate();
    await new Promise<void>((resolve) => this.http.close(() => resolve()));
  }

  /** Every token but "B" stops standing, as a gateway's tokens do, and the open sockets end. */
  expire(): void {
    this.authorize = (token) => token === "B";
    this.dropAll();
  }

  dropAll(): void {
    for (const ws of this.sockets.clients) ws.close(1001, "the token expired or was revoked");
  }

  liveQueries(): number {
    let total = 0;
    for (const conn of this.conns.values()) total += conn.live.size;
    return total;
  }

  private accept(ws: WebSocket): void {
    const conn: Conn = { version: 0, live: new Set() };
    this.conns.set(ws, conn);
    ws.on("close", () => this.conns.delete(ws));
    ws.on("message", (data) => {
      const msg = JSON.parse(data.toString()) as {
        type: string;
        requestId?: number;
        modifications?: { type: string; queryId: number }[];
      };
      if (msg.type === "ModifyQuerySet") {
        const items = (msg.modifications ?? []).map((mod) => {
          if (mod.type === "Remove") {
            conn.live.delete(mod.queryId);
            return { type: "QueryRemoved", queryId: mod.queryId };
          }
          conn.live.add(mod.queryId);
          this.adds.push(mod.queryId);
          return { type: "QueryUpdated", queryId: mod.queryId, value: { n: this.value }, logLines: [] };
        });
        const start = conn.version;
        conn.version += 1;
        ws.send(
          JSON.stringify({ type: "Transition", startVersion: start, endVersion: conn.version, modifications: items }),
        );
      } else if (msg.type === "Mutation") {
        this.mutations += 1;
        if (this.mute) return;
        this.value += 1;
        ws.send(
          JSON.stringify({
            type: "MutationResponse",
            requestId: msg.requestId,
            success: true,
            result: { n: this.value },
            ts: "1",
          }),
        );
        this.push();
      }
    });
  }

  private push(): void {
    for (const [ws, conn] of this.conns) {
      if (conn.live.size === 0) continue;
      const items = [...conn.live].map((queryId) => ({
        type: "QueryUpdated",
        queryId,
        value: { n: this.value },
        logLines: [],
      }));
      const start = conn.version;
      conn.version += 1;
      ws.send(
        JSON.stringify({ type: "Transition", startVersion: start, endVersion: conn.version, modifications: items }),
      );
    }
  }
}

async function until(what: string, cond: () => boolean, timeoutMs = 5000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (cond()) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`timed out waiting for ${what}`);
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

const n = (value: unknown) => (value as { n: number }).n;

describe("token source", () => {
  let server: CounterSync | null = null;
  const clients: Client[] = [];
  afterEach(async () => {
    for (const client of clients.splice(0)) await client.close();
    await server?.stop();
    server = null;
  });

  async function counter(): Promise<CounterSync> {
    server = new CounterSync();
    await server.start();
    return server;
  }

  async function dialWith(url: string, token: Parameters<typeof dial>[1]["token"]): Promise<Client> {
    const client = await dial(url, { token });
    clients.push(client);
    return client;
  }

  it("renews the session across a redial", async () => {
    const s = await counter();
    s.authorize = (token) => token === "A";
    // The gateway hands out A until the server has refused it, then B, as a cached token does.
    const source = async () => (s.refused ? "B" : "A");
    const client = await dialWith(s.url, source);
    const watch = await client.watch("Thing/read", { id: "1" });

    // The token runs out: the server drops the socket and refuses A from now on.
    s.expire();
    await until("the session to dial again with B", () => s.tokens.length >= 3 && client.connected);
    // A opened the session; the redial tried A again, was refused, asked the source once more and
    // dialled with B.
    expect(s.tokens).toEqual(["A", "A", "B"]);

    // The watch was resubscribed under the same id: it is delivered again, and a write made after
    // the redial reaches it.
    expect(n(await watch.next())).toBe(0);
    expect(await client.mutate("Thing/update", { id: "1" })).toEqual({ n: 1 });
    expect(n(await watch.next())).toBe(1);
    expect(s.adds).toEqual([1, 1]);
  }, 20_000);

  it("ends the session with a static token, as before", async () => {
    const s = await counter();
    s.authorize = (token) => token === "A";
    const client = await dialWith(s.url, "A");
    const watch = await client.watch("Thing/read", { id: "1" });
    s.expire();
    await expect(watch.next()).rejects.toMatchObject({ name: "UpgradeError", status: 401 });
    await expect(client.mutate("Thing/update", {})).rejects.toBeInstanceOf(UpgradeError);
    // A constant token is not asked again for the token it was just refused with.
    expect(s.tokens).toEqual(["A", "A"]);
  }, 10_000);

  it("treats a second 401 with a renewed token as final", async () => {
    const s = await counter();
    s.authorize = (token) => token === "A";
    let asked = 0;
    const source = async () => ["A", "B", "C"][asked++] ?? "D";
    const client = await dialWith(s.url, source);
    const watch = await client.watch("Thing/read", { id: "1" });
    s.dropAll();
    await expect(watch.next()).rejects.toMatchObject({ name: "UpgradeError", status: 401 });
    // Asked for the first dial (A), the redial (B) and once more after its 401 (C): no more.
    await sleep(300);
    expect(asked).toBe(3);
    await expect(client.mutate("Thing/update", {})).rejects.toBeInstanceOf(UpgradeError);
  }, 10_000);

  it("treats 403 as final without asking again", async () => {
    const s = await counter();
    let asked = 0;
    const source = async () => String(asked++);
    const client = await dialWith(s.url, source);
    const watch = await client.watch("Thing/read", { id: "1" });
    s.status = 403;
    s.dropAll();
    await expect(watch.next()).rejects.toMatchObject({ name: "UpgradeError", status: 403 });
    expect(asked).toBe(2);
  }, 10_000);

  it("retries an error from the source instead of ending the session", async () => {
    const s = await counter();
    let asked = 0;
    const source = async () => {
      // The first call is the dial; the next two fail, as a gateway that is briefly down does.
      asked += 1;
      if (asked === 2 || asked === 3) throw new Error("the gateway is down");
      return "A";
    };
    const client = await dialWith(s.url, source);
    const watch = await client.watch("Thing/read", { id: "1" });
    s.dropAll();
    expect(n(await watch.next())).toBe(0);
    expect(asked).toBe(4);
    expect(await client.mutate("Thing/update", { id: "1" })).toEqual({ n: 1 });
  }, 20_000);

  it("rejects dial with the source's error", async () => {
    const s = await counter();
    await expect(
      dial(s.url, {
        token: async () => {
          throw new Error("no token today");
        },
      }),
    ).rejects.toThrow("no token today");
  });

  it("is asked for every REST call", async () => {
    const s = await counter();
    let asked = 0;
    const client = await dialWith(s.url, async () => `token-${++asked}`);
    await client.metadata();
    await client.metadata();
    // Token 1 opened the socket; each REST call asks again.
    expect(s.restTokens).toEqual(["Bearer token-2", "Bearer token-3"]);
  });
});

describe("calls while the client dials again", () => {
  let server: CounterSync | null = null;
  const clients: Client[] = [];
  afterEach(async () => {
    for (const client of clients.splice(0)) await client.close();
    await server?.stop();
    server = null;
  });

  async function connected(): Promise<{ s: CounterSync; client: Client }> {
    const s = new CounterSync();
    server = s;
    await s.start();
    const client = await dial(s.url);
    clients.push(client);
    return { s, client };
  }

  /** Drop the socket and keep the client dialling: the server turns every upgrade away. */
  async function redialling(s: CounterSync, client: Client): Promise<void> {
    s.status = 503;
    s.dropAll();
    await until("the client to notice the drop", () => !client.connected);
  }

  it("fails a mutation at once with NotSentError, and never sends it", async () => {
    const { s, client } = await connected();
    const watch = await client.watch("Thing/read", { id: "1" });
    await redialling(s, client);

    const started = Date.now();
    const failure = await client.mutate("Thing/update", { id: "1" }).catch((err: unknown) => err);
    expect(failure).toBeInstanceOf(NotSentError);
    expect(failure).not.toBeInstanceOf(DisconnectedError);
    expect(Date.now() - started).toBeLessThan(1000);

    s.status = 0;
    await until("the client to be back", () => client.connected);
    expect(n(await watch.next())).toBe(0);
    await sleep(200);
    expect(s.mutations).toBe(0);
    // Once the client is back, it writes again.
    expect(await client.mutate("Thing/update", { id: "1" })).toEqual({ n: 1 });
  }, 20_000);

  it("fails a mutation lost with the socket as DisconnectedError, which may have been applied", async () => {
    const { s, client } = await connected();
    s.mute = true;
    const lost = client.mutate("Thing/update", { id: "1" }).catch((err: unknown) => err);
    await until("the server to receive it", () => s.mutations === 1);
    s.status = 503;
    s.dropAll();
    const failure = await lost;
    expect(failure).toBeInstanceOf(DisconnectedError);
    expect(failure).not.toBeInstanceOf(NotSentError);
  }, 10_000);

  it("does not send a mutation whose signal has aborted", async () => {
    const { s, client } = await connected();
    const gone = AbortSignal.abort();
    await expect(client.mutate("Thing/update", { id: "1" }, { signal: gone })).rejects.toBeInstanceOf(
      NotSentError,
    );
    await sleep(100);
    expect(s.mutations).toBe(0);
  });

  it("rejects a call waiting for its answer when the signal aborts", async () => {
    const { s, client } = await connected();
    s.mute = true;
    const controller = new AbortController();
    const waiting = client.mutate("Thing/update", { id: "1" }, { signal: controller.signal });
    await until("the server to receive it", () => s.mutations === 1);
    controller.abort(new Error("gave up"));
    await expect(waiting).rejects.toThrow("gave up");
  });

  it("does not add a watch whose signal aborted while the client dialled again", async () => {
    const { s, client } = await connected();
    const first = await client.watch("Thing/read", { id: "1" });
    await redialling(s, client);

    const controller = new AbortController();
    const waiting = client.watch("Thing/read", { id: "2" }, { signal: controller.signal });
    const outcome = waiting.catch((err: unknown) => err);
    await sleep(100);
    controller.abort(new Error("gave up"));
    expect(await outcome).toMatchObject({ message: "gave up" });

    s.status = 0;
    await until("the client to be back", () => client.connected);
    await first.next();
    // The session has settled when a write reaches the surviving watch.
    await client.mutate("Thing/update", { id: "1" });
    await first.next();
    expect(s.liveQueries()).toBe(1);
  }, 20_000);
});

describe("QueryFailed codes", () => {
  let http: Server | null = null;
  const sockets = new WebSocketServer({ noServer: true });
  afterEach(async () => {
    for (const ws of sockets.clients) ws.terminate();
    await new Promise<void>((resolve) => (http ? http.close(() => resolve()) : resolve()));
    http = null;
  });

  async function failing(items: Record<string, unknown>[]): Promise<Client> {
    http = createServer();
    http.on("upgrade", (req, socket: Duplex, head) => {
      sockets.handleUpgrade(req, socket, head, (ws) => {
        ws.once("message", () => {
          ws.send(
            JSON.stringify({ type: "Transition", startVersion: 0, endVersion: 1, modifications: items }),
          );
        });
      });
    });
    await new Promise<void>((resolve) => http!.listen(0, "127.0.0.1", resolve));
    return dial(`http://127.0.0.1:${(http.address() as AddressInfo).port}`);
  }

  it("gives watch a QueryError carrying the code", async () => {
    const client = await failing([
      { type: "QueryFailed", queryId: 1, error: "Patient p1 was deleted", code: "deleted", logLines: [] },
    ]);
    try {
      const failure = await client.watch("Patient/read", { id: "p1" }).catch((err: unknown) => err);
      expect(failure).toBeInstanceOf(QueryError);
      expect(failure).toMatchObject({
        path: "Patient/read",
        code: "deleted",
        detail: "Patient p1 was deleted",
        message: "refhir: Patient/read: Patient p1 was deleted",
      });
    } finally {
      await client.close();
    }
  });

  it("leaves the code empty for a server that sends none", async () => {
    const client = await failing([{ type: "QueryFailed", queryId: 1, error: "nope", logLines: [] }]);
    try {
      const failure = await client.watch("Patient/read", { id: "p1" }).catch((err: unknown) => err);
      expect(failure).toBeInstanceOf(QueryError);
      expect((failure as QueryError).code).toBe("");
    } finally {
      await client.close();
    }
  });
});

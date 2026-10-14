import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import type { Duplex } from "node:stream";

import { afterEach, describe, expect, it } from "vitest";
import { WebSocketServer, type WebSocket } from "ws";

import { dial, HttpError, UpgradeError } from "../src/index.js";
import { parseRetryAfter } from "../src/errors.js";

/**
 * A /sync server that does what a test tells it, to reach the moments a real server only reaches
 * by a race: a push that lands before the answer to a change, and an upgrade that is refused after
 * the session was up.
 */
class ScriptedSync {
  readonly attempts: number[] = [];
  readonly bases: number[] = [];
  /** How to turn away upgrade n (counted from 1): a status and a Retry-After. 0 accepts it. */
  refuse: (n: number) => [number, string?] = () => [0];
  /** Runs an accepted connection. */
  serve: (ws: WebSocket, n: number) => void = () => {};

  private readonly http: Server;
  private readonly sockets = new WebSocketServer({ noServer: true });
  url = "";

  constructor() {
    this.http = createServer();
    this.http.on("upgrade", (req, socket: Duplex, head) => {
      this.attempts.push(Date.now());
      const n = this.attempts.length;
      const [status, retryAfter] = this.refuse(n);
      if (status !== 0) {
        const extra = retryAfter ? `Retry-After: ${retryAfter}\r\n` : "";
        socket.write(`HTTP/1.1 ${status} Refused\r\n${extra}Content-Length: 0\r\nConnection: close\r\n\r\n`);
        socket.destroy();
        return;
      }
      this.sockets.handleUpgrade(req, socket, head, (ws) => this.serve(ws, n));
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

  /** Read a ModifyQuerySet, recording its base. */
  query(raw: string): { ids: number[]; kinds: string[] } {
    const msg = JSON.parse(raw) as {
      baseVersion: number;
      modifications: { type: string; queryId: number }[];
    };
    this.bases.push(msg.baseVersion);
    return {
      ids: msg.modifications.map((m) => m.queryId),
      kinds: msg.modifications.map((m) => m.type),
    };
  }
}

/** A Transition holding one item for each modification. */
function transition(start: number, end: number, ids: number[], kinds: string[]): string {
  return JSON.stringify({
    type: "Transition",
    startVersion: start,
    endVersion: end,
    modifications: ids.map((id, i) =>
      kinds[i] === "Remove"
        ? { type: "QueryRemoved", queryId: id }
        : { type: "QueryUpdated", queryId: id, value: { query: id }, logLines: [] },
    ),
  });
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

describe("sync gaps", () => {
  let server: ScriptedSync | null = null;
  afterEach(async () => {
    await server?.stop();
    server = null;
  });

  async function scripted(): Promise<ScriptedSync> {
    server = new ScriptedSync();
    await server.start();
    return server;
  }

  it("accepts the answer to a change that follows a push", async () => {
    const s = await scripted();
    s.serve = (ws) => {
      let version = 0;
      let change = 0;
      ws.on("message", (data) => {
        const { ids, kinds } = s.query(data.toString());
        if (change === 1) {
          // A write landed between the client sending this change and the server reading it: the
          // push goes out first, so the answer is not at the version the change was based on.
          ws.send(transition(version, version + 1, [1], ["Add"]));
          version += 1;
        }
        change += 1;
        const start = version;
        version += 1;
        ws.send(transition(start, version, ids, kinds));
      });
    };
    const client = await dial(s.url);
    try {
      // One after another: each change is sent once the one before has been answered, so a
      // session that never saw the answer to the second one never sends the third.
      for (let i = 0; i < 3; i++) {
        await client.watch("Patient/search", {});
      }
    } finally {
      await client.close();
    }
    // The third change is based on the version after the push and the answer.
    expect(s.bases.slice(0, 3)).toEqual([0, 1, 3]);
  }, 10_000);

  it("ends the session when the server refuses the token, instead of retrying", async () => {
    const s = await scripted();
    s.refuse = (n) => (n === 1 ? [0] : [401]);
    s.serve = (ws) => {
      ws.once("message", (data) => {
        const { ids, kinds } = s.query(data.toString());
        ws.send(transition(0, 1, ids, kinds));
        // The token is revoked: the server ends the connection, and refuses the next upgrade.
        ws.close(1008, "the token expired or was revoked");
      });
    };
    const client = await dial(s.url, { token: "old" });
    try {
      const watch = await client.watch("Patient/search", {});
      await expect(watch.next()).rejects.toMatchObject({ name: "UpgradeError", status: 401 });
      // The session is over: it says why to whoever asks next, and does not dial again.
      await expect(client.mutate("Patient/create", {})).rejects.toBeInstanceOf(UpgradeError);
      await expect(client.watch("Patient/search", {})).rejects.toBeInstanceOf(HttpError);
      await sleep(700);
      expect(s.attempts.length).toBe(2);
    } finally {
      await client.close();
    }
  }, 10_000);

  it("rejects dial with the refusal when the first upgrade is refused", async () => {
    const s = await scripted();
    s.refuse = () => [401];
    await expect(dial(s.url, { token: "bad" })).rejects.toMatchObject({
      name: "UpgradeError",
      status: 401,
    });
  });

  it("holds back the next dial for the Retry-After the server asked for", async () => {
    const s = await scripted();
    s.refuse = (n) => (n === 2 ? [429, "1"] : [0]);
    s.serve = (ws, n) => {
      ws.on("message", (data) => {
        const { ids, kinds } = s.query(data.toString());
        ws.send(transition(0, 1, ids, kinds));
        if (n === 1) ws.close(1001, "");
      });
    };
    const client = await dial(s.url);
    try {
      const watch = await client.watch("Patient/search", {});
      // The first connection closes; the second upgrade is turned away with Retry-After: 1; the
      // third works, and the watch is delivered again on it.
      await watch.next();
      expect(s.attempts.length).toBeGreaterThanOrEqual(3);
      expect(s.attempts[2] - s.attempts[1]).toBeGreaterThanOrEqual(900);
    } finally {
      await client.close();
    }
  }, 15_000);

  it("reads Retry-After as seconds or a date, capped", () => {
    expect(parseRetryAfter(undefined)).toBe(0);
    expect(parseRetryAfter("")).toBe(0);
    expect(parseRetryAfter("3")).toBe(3000);
    expect(parseRetryAfter("  5 ")).toBe(5000);
    expect(parseRetryAfter("-2")).toBe(0);
    expect(parseRetryAfter("soon")).toBe(0);
    expect(parseRetryAfter("100000")).toBe(60_000);
    const ahead = parseRetryAfter(new Date(Date.now() + 20_000).toUTCString());
    expect(ahead).toBeGreaterThan(15_000);
    expect(ahead).toBeLessThanOrEqual(21_000);
    expect(parseRetryAfter(new Date(Date.now() - 3_600_000).toUTCString())).toBe(0);
  });
});

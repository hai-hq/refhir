import { createServer, type IncomingMessage, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterEach, describe, expect, it } from "vitest";
import { WebSocketServer } from "ws";

import { dial } from "../src/index.js";

/**
 * Fake ReFHIR origin: REST answers carry a scripted X-Refhir-Commit-Ts, and
 * /sync answers mutations with a scripted ts. No refhir binary involved.
 */
class FakeServer {
  readonly seen: { method: string; url: string; minTs: string | undefined }[] = [];
  restTs: (string | undefined)[] = [];
  mutateTs: (string | null | undefined)[] = [];
  syncConnections = 0;

  private readonly http: Server;
  private readonly wss: WebSocketServer;
  port = 0;

  constructor() {
    this.http = createServer((req, res) => this.rest(req, res));
    this.wss = new WebSocketServer({ server: this.http, path: "/sync" });
    this.wss.on("connection", (socket) => {
      this.syncConnections += 1;
      socket.on("message", (data) => {
        const msg = JSON.parse(data.toString()) as { type: string; requestId?: number };
        if (msg.type !== "Mutation") return;
        const ts = this.mutateTs.shift();
        socket.send(
          JSON.stringify({
            type: "MutationResponse",
            requestId: msg.requestId,
            success: true,
            result: {},
            ts: ts === undefined ? null : ts,
          }),
        );
      });
    });
  }

  get base(): string {
    return `http://127.0.0.1:${this.port}`;
  }

  async start(port = 0): Promise<void> {
    await new Promise<void>((resolve, reject) => {
      this.http.once("error", reject);
      this.http.listen(port, "127.0.0.1", () => {
        this.http.off("error", reject);
        resolve();
      });
    });
    this.port = (this.http.address() as AddressInfo).port;
  }

  /** Drop every connection and stop listening, as a failing-over server would. */
  async stop(): Promise<void> {
    for (const socket of this.wss.clients) socket.terminate();
    this.http.closeAllConnections();
    await new Promise<void>((resolve) => this.http.close(() => resolve()));
  }

  private rest(req: IncomingMessage, res: import("node:http").ServerResponse): void {
    const raw = req.headers["x-refhir-min-ts"];
    this.seen.push({
      method: req.method ?? "",
      url: req.url ?? "",
      minTs: Array.isArray(raw) ? raw[0] : raw,
    });
    req.resume();
    const ts = this.restTs.shift();
    if (ts !== undefined) res.setHeader("X-Refhir-Commit-Ts", ts);
    res.setHeader("Content-Type", "application/fhir+json");
    res.end("{}");
  }

  minTsSeen(): (string | undefined)[] {
    return this.seen.map((r) => r.minTs);
  }
}

let fake: FakeServer | undefined;
const clients: { close(): Promise<void> }[] = [];

async function setup(options: { readYourWrites?: boolean } = {}) {
  fake = new FakeServer();
  await fake.start();
  const client = await dial(fake.base, options);
  clients.push(client);
  return { fake, client };
}

afterEach(async () => {
  for (const client of clients.splice(0)) await client.close();
  await fake?.stop();
  fake = undefined;
});

const body = new TextEncoder().encode('{"resourceType":"Patient"}');

describe("read-your-writes header", () => {
  it("captures X-Refhir-Commit-Ts from a write and sends it on the next request", async () => {
    const { fake, client } = await setup();
    fake.restTs = ["100", undefined, undefined];

    expect(client.lastCommitTs).toBe(0n);
    await client.doRequest("POST", "/Patient", body);
    expect(client.lastCommitTs).toBe(100n);
    // Later requests carry it, including ones whose response has no header.
    await client.doRequest("GET", "/Patient/p1");
    await client.doRequest("GET", "Patient/p1");

    // The first request had nothing to send; the others send 100.
    expect(fake.minTsSeen()).toEqual([undefined, "100", "100"]);
  });

  it("keeps the maximum and ignores malformed values", async () => {
    const { fake, client } = await setup();
    // 500, stale 300 (must not lower), 700 (raises), then garbage.
    fake.restTs = ["500", "300", "700", "abc", "-1", "", "12x"];

    await client.doRequest("GET", "/a");
    await client.doRequest("GET", "/b");
    expect(client.lastCommitTs).toBe(500n);
    await client.doRequest("GET", "/c");
    expect(client.lastCommitTs).toBe(700n);
    for (const path of ["/d", "/e", "/f", "/g"]) await client.doRequest("GET", path);
    expect(client.lastCommitTs).toBe(700n);

    expect(fake.minTsSeen()).toEqual([
      undefined,
      "500",
      "500",
      "700",
      "700",
      "700",
      "700",
    ]);
  });

  it("keeps full u64 precision", async () => {
    const { fake, client } = await setup();
    const max = "18446744073709551615";
    fake.restTs = [max, undefined];

    await client.doRequest("GET", "/a");
    await client.doRequest("GET", "/b");
    expect(client.lastCommitTs).toBe(18446744073709551615n);
    expect(fake.minTsSeen()[1]).toBe(max);
  });

  it("records ts from /sync mutation responses and feeds REST", async () => {
    const { fake, client } = await setup();
    fake.mutateTs = ["250", "200", null];

    await client.create("Patient", { resourceType: "Patient" });
    expect(client.lastCommitTs).toBe(250n);
    // A smaller ts later does not lower it; a null ts changes nothing.
    await client.delete("Patient", "p1");
    await client.delete("Patient", "p2");
    expect(client.lastCommitTs).toBe(250n);

    await client.doRequest("GET", "/Patient/p1");
    expect(fake.minTsSeen()).toEqual(["250"]);
  });

  it("sends nothing when readYourWrites is false, but still tracks", async () => {
    const { fake, client } = await setup({ readYourWrites: false });
    fake.restTs = ["100", "200", undefined];
    fake.mutateTs = ["300"];

    await client.doRequest("GET", "/a");
    await client.create("Patient", { resourceType: "Patient" });
    await client.doRequest("GET", "/b");
    await client.doRequest("GET", "/c");

    expect(fake.minTsSeen()).toEqual([undefined, undefined, undefined]);
    expect(client.lastCommitTs).toBe(300n);
  });

  it("requestWithHeaders exposes the response headers", async () => {
    const { fake, client } = await setup();
    fake.restTs = ["42"];

    const response = await client.requestWithHeaders("GET", "/Patient/p1");
    expect(response.status).toBe(200);
    expect(response.headers.get("x-refhir-commit-ts")).toBe("42");
    expect(new TextDecoder().decode(response.body)).toBe("{}");
    expect(client.lastCommitTs).toBe(42n);
  });
});

describe("reconnect", () => {
  it("retries with backoff until the server is back", async () => {
    const { fake, client } = await setup();
    const port = fake.port;
    fake.mutateTs = [];
    expect(fake.syncConnections).toBe(1);

    await fake.stop();
    // Let the client notice and fail a few dial attempts.
    await new Promise((resolve) => setTimeout(resolve, 400));
    await expect(client.mutate("Patient/create", {})).rejects.toThrow(/disconnected/);

    await fake.start(port);
    fake.mutateTs = ["900"];
    const deadline = Date.now() + 10_000;
    for (;;) {
      try {
        await client.mutate("Patient/create", {});
        break;
      } catch (err) {
        if (Date.now() > deadline) throw err;
        await new Promise((resolve) => setTimeout(resolve, 50));
      }
    }
    expect(fake.syncConnections).toBe(2);
    expect(client.lastCommitTs).toBe(900n);
  });

  it("stops retrying when closed", async () => {
    const { fake, client } = await setup();
    const port = fake.port;
    await fake.stop();
    await new Promise((resolve) => setTimeout(resolve, 200));
    await client.close();

    // Bring the server back: a closed client must not dial it again.
    await fake.start(port);
    await new Promise((resolve) => setTimeout(resolve, 300));
    expect(fake.syncConnections).toBe(1);
    await expect(client.mutate("Patient/create", {})).rejects.toThrow();
  });
});

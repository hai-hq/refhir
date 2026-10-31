import { describe, expect, it } from "vitest";

import { dial, OutcomeError, QueryError } from "../src/index.js";
import { modifyQuerySet, mutationMessage } from "../src/protocol.js";
import { attachAccessToken, splitBase } from "../src/url.js";
import { freePort, startServer } from "./server.js";

describe("wire", () => {
  it("uses the sync protocol field names", () => {
    const q = modifyQuerySet(0, 1, [
      { type: "Add", queryId: 1, udfPath: "Patient/search", args: { active: "true" } },
    ]);
    expect(q).toMatchObject({
      type: "ModifyQuerySet",
      baseVersion: 0,
      newVersion: 1,
    });
    const m = mutationMessage(4, "Patient/update", { id: "p1" });
    expect(m).toMatchObject({ type: "Mutation", requestId: 4, udfPath: "Patient/update" });
    const { httpBase, wsURL } = splitBase("http://127.0.0.1:8080");
    expect(httpBase).toBe("http://127.0.0.1:8080");
    expect(wsURL).toBe("ws://127.0.0.1:8080/sync");
    expect(attachAccessToken(wsURL, "secret")).toBe(
      "ws://127.0.0.1:8080/sync?access_token=secret",
    );
  });
});

describe("integration", () => {
  it("search follows mutation on another connection", async () => {
    const port = await freePort();
    const { base, stop } = await startServer(port);
    const reader = await dial(base);
    const writer = await dial(base);
    try {
      await reader.metadata();
      // The server assigns the id of a resource it creates, whatever the body says.
      const created = (await reader.create("Patient", {
        resourceType: "Patient",
        active: true,
        name: [{ family: "Ann" }],
      })) as { id: string };
      const watch = await reader.search("Patient", { active: "true" });
      expect(family(watch.latest())).toBe("Ann");

      await writer.update("Patient", created.id, {
        resourceType: "Patient",
        id: created.id,
        active: true,
        name: [{ family: "Bee" }],
      });
      const next = await watch.next();
      expect(family(next)).toBe("Bee");

      const one = await reader.query("Patient/read", { id: created.id });
      expect(family(one)).toBe("Bee");
    } finally {
      await reader.close();
      await writer.close();
      stop();
    }
  });

  it("returns OperationOutcome on failed update", async () => {
    const port = await freePort();
    const { base, stop } = await startServer(port);
    const client = await dial(base);
    try {
      await expect(
        client.update("Patient", "missing", {
          resourceType: "Patient",
          id: "missing",
          active: true,
        }),
      ).rejects.toBeInstanceOf(OutcomeError);
    } finally {
      await client.close();
      stop();
    }
  });

  it("fails a watch whose resource was deleted with code deleted", async () => {
    const port = await freePort();
    const { base, stop } = await startServer(port);
    const watcher = await dial(base);
    const writer = await dial(base);
    try {
      const created = (await writer.create("Patient", {
        resourceType: "Patient",
        active: true,
        name: [{ family: "Ann" }],
      })) as { id: string };
      const watch = await watcher.read("Patient", created.id);
      await writer.delete("Patient", created.id);
      const failure = await watch.next().catch((err: unknown) => err);
      expect(failure).toBeInstanceOf(QueryError);
      expect(failure).toMatchObject({ code: "deleted", path: "Patient/read" });
    } finally {
      await watcher.close();
      await writer.close();
      stop();
    }
  });
});

function family(value: unknown): string {
  const v = value as {
    entry?: { resource?: { name?: { family?: string }[] } }[];
    name?: { family?: string }[];
  };
  if (v.entry?.[0]?.resource?.name?.[0]?.family) {
    return v.entry[0].resource!.name![0].family!;
  }
  return v.name?.[0]?.family ?? "";
}

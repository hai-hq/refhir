import { describe, expect, it } from "vitest";

import { dial, OutcomeError } from "../src/index.js";
import { modifyQuerySet, mutationMessage } from "../src/protocol.js";
import { attachAccessToken, splitBase } from "../src/url.js";
import { freePort, startServer } from "./server.js";

describe("wire", () => {
  it("uses Convex field names", () => {
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
      await reader.create("Patient", {
        resourceType: "Patient",
        id: "p1",
        active: true,
        name: [{ family: "Ann" }],
      });
      const watch = await reader.search("Patient", { active: "true" });
      expect(family(watch.latest())).toBe("Ann");

      await writer.update("Patient", "p1", {
        resourceType: "Patient",
        id: "p1",
        active: true,
        name: [{ family: "Bee" }],
      });
      const next = await watch.next();
      expect(family(next)).toBe("Bee");

      const one = await reader.query("Patient/read", { id: "p1" });
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

import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { loadResources, transactionBundle } from "../src/seed/load.js";

const library = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "..",
  "fixtures",
  "mock-fhir",
  "library",
);

describe("seed/load", () => {
  it("loads demo library in upload order", async () => {
    const resources = await loadResources(library);
    expect(resources.length).toBeGreaterThan(5);
    expect(resources[0].resourceType).toBe("Organization");
    const bundle = transactionBundle(resources.slice(0, 2));
    expect(bundle.resourceType).toBe("Bundle");
    expect(bundle.type).toBe("transaction");
    expect((bundle as { entry: unknown[] }).entry).toHaveLength(2);
    // The data names its own ids, so each resource is an upsert at that id.
    const entries = (bundle as { entry: { request: { method: string; url: string } }[] }).entry;
    for (const [index, entry] of entries.entries()) {
      expect(entry.request.method).toBe("PUT");
      expect(entry.request.url).toBe(`${resources[index].resourceType}/${resources[index].id}`);
    }
  });

  it("names the file when JSON is invalid or an id is missing", async () => {
    const dir = await mkdtemp(path.join(tmpdir(), "refhir-seed-"));
    try {
      const bad = path.join(dir, "bad.json");
      await writeFile(bad, "{");
      await expect(loadResources(dir)).rejects.toThrow(/bad\.json/);

      await rm(bad);
      const missing = path.join(dir, "patient.json");
      await writeFile(missing, JSON.stringify({ resourceType: "Patient" }));
      await expect(loadResources(dir)).rejects.toThrow(/patient\.json: Patient missing id/);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

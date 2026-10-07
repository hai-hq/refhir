#!/usr/bin/env node
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { OutcomeError } from "../src/errors.js";
import { dial } from "../src/dial.js";
import { chunkResources, loadResources, transactionBundle } from "../src/seed/load.js";
import type { JsonValue } from "../src/types.js";

function parseArgs(argv: string[]): Record<string, string> {
  const out: Record<string, string> = {};
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (!arg.startsWith("-")) continue;
    const key = arg.replace(/^-+/, "");
    const next = argv[i + 1];
    if (next && !next.startsWith("-")) {
      out[key] = next;
      i++;
    } else {
      out[key] = "true";
    }
  }
  return out;
}

function defaultDataDir(): string {
  const here = path.dirname(fileURLToPath(import.meta.url));
  const candidates = [
    "fixtures/mock-fhir/library",
    path.join(here, "..", "..", "..", "fixtures", "mock-fhir", "library"),
    path.join(here, "..", "fixtures", "mock-fhir", "library"),
  ];
  for (const dir of candidates) {
    if (existsSync(dir)) return dir;
  }
  return "fixtures/mock-fhir/library";
}

function summarize(response: JsonValue): string[] {
  if (response === null || typeof response !== "object" || Array.isArray(response)) {
    return [JSON.stringify(response)];
  }
  const bundle = response as {
    entry?: {
      resource?: { resourceType?: string; id?: string };
      response?: { status?: string };
    }[];
  };
  const lines: string[] = [];
  for (const entry of bundle.entry ?? []) {
    const id = entry.resource?.id;
    if (!id) continue;
    lines.push(
      `${entry.resource?.resourceType}/${id} (${entry.response?.status ?? "?"})`,
    );
  }
  return lines;
}

async function main(): Promise<void> {
  const args = parseArgs(process.argv.slice(2));
  const base = args.url ?? "http://127.0.0.1:8080";
  const dir = path.resolve(args.dir ?? defaultDataDir());
  const token = args.token;
  const batch = Number(args.batch ?? "40");
  const timeoutMs = Number(args.timeout ?? String(10 * 60 * 1000));

  const resources = await loadResources(dir);
  if (resources.length === 0) {
    throw new Error(`no FHIR resources found under ${dir}`);
  }

  const client = await dial(base, token ? { token } : {});
  const timer = setTimeout(() => {
    void client.close();
  }, timeoutMs);

  try {
    await client.metadata();
    let created = 0;
    const batches = chunkResources(resources, batch);
    for (let i = 0; i < batches.length; i++) {
      try {
        const result = await client.transaction(transactionBundle(batches[i]));
        created += summarize(result).length;
      } catch (err) {
        if (err instanceof OutcomeError) {
          throw new Error(
            `batch ${i + 1} failed (duplicate ids or validation): ${JSON.stringify(err.outcome)}`,
          );
        }
        throw new Error(`batch ${i + 1}: ${err instanceof Error ? err.message : err}`);
      }
    }
    console.log(
      `Loaded ${resources.length} resources from ${dir} (${batches.length} transaction batches).`,
    );
    console.log(`Server reported ${created} created entries.`);
  } finally {
    clearTimeout(timer);
    await client.close();
  }
}

main().catch((err) => {
  console.error(err instanceof Error ? err.message : err);
  process.exit(1);
});

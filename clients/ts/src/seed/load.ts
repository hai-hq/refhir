import { readdir, readFile, stat } from "node:fs/promises";
import path from "node:path";

import type { JsonValue } from "../types.js";

const uploadRank: Record<string, number> = {
  Organization: 10,
  Location: 20,
  HealthcareService: 30,
  Practitioner: 40,
  Patient: 50,
  RelatedPerson: 60,
  PractitionerRole: 70,
  Encounter: 80,
  Condition: 90,
  Observation: 100,
  Procedure: 110,
  MedicationRequest: 120,
  DiagnosticReport: 130,
  Immunization: 140,
  AllergyIntolerance: 150,
  CarePlan: 160,
  Claim: 170,
  DocumentReference: 180,
  ExplanationOfBenefit: 190,
};

export async function loadResources(dir: string): Promise<Record<string, JsonValue>[]> {
  const info = await stat(dir);
  if (!info.isDirectory()) {
    throw new Error(`${dir} is not a directory`);
  }

  const files: string[] = [];
  await walkJson(dir, files);
  files.sort();

  const byKey = new Map<string, Record<string, JsonValue>>();
  for (const file of files) {
    const data = await readFile(file, "utf8");
    for (const resource of parseJSONFile(data, file)) {
      const key = resourceKey(resource, file);
      byKey.set(key, resource);
    }
  }

  const keys = [...byKey.keys()].sort((a, b) => {
    const ri = rank(a);
    const rj = rank(b);
    if (ri !== rj) return ri - rj;
    return a.localeCompare(b);
  });

  return keys.map((key) => byKey.get(key)!);
}

async function walkJson(dir: string, out: string[]): Promise<void> {
  const entries = await readdir(dir, { withFileTypes: true });
  for (const entry of entries) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      await walkJson(full, out);
    } else if (entry.isFile() && entry.name.toLowerCase().endsWith(".json")) {
      out.push(full);
    }
  }
}

function parseJSONFile(data: string, file: string): Record<string, JsonValue>[] {
  try {
    const raw = JSON.parse(data) as JsonValue;
    return flattenAny(raw);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    throw new Error(`${file}: invalid JSON: ${message}`);
  }
}

function flattenAny(raw: JsonValue): Record<string, JsonValue>[] {
  if (raw !== null && typeof raw === "object" && !Array.isArray(raw)) {
    const obj = raw as Record<string, JsonValue>;
    if (obj.resourceType === "Bundle") {
      return flattenBundle(obj);
    }
    if (typeof obj.resourceType === "string") {
      return [obj];
    }
  }
  if (Array.isArray(raw)) {
    return raw.flatMap((item) => flattenAny(item));
  }
  return [];
}

function flattenBundle(bundle: Record<string, JsonValue>): Record<string, JsonValue>[] {
  const entries = bundle.entry;
  if (!Array.isArray(entries)) return [];
  const out: Record<string, JsonValue>[] = [];
  for (const entry of entries) {
    if (entry === null || typeof entry !== "object" || Array.isArray(entry)) continue;
    const item = entry as Record<string, JsonValue>;
    const resource = item.resource;
    if (resource === null || typeof resource !== "object" || Array.isArray(resource)) continue;
    const res = resource as Record<string, JsonValue>;
    const rt = res.resourceType;
    if (typeof rt === "string" && rt !== "" && rt !== "Bundle") {
      out.push(res);
    }
  }
  return out;
}

function resourceKey(resource: Record<string, JsonValue>, file: string): string {
  const rt = resource.resourceType;
  if (typeof rt !== "string" || rt === "") {
    throw new Error(`${file}: missing resourceType`);
  }
  const id = resource.id;
  if (typeof id !== "string" || id === "") {
    throw new Error(`${file}: ${rt} missing id`);
  }
  return `${rt}/${id}`;
}

function rank(key: string): number {
  const rt = key.split("/")[0] ?? "";
  return uploadRank[rt] ?? 500;
}

export function transactionBundle(resources: Record<string, JsonValue>[]): JsonValue {
  const entries = resources.map((resource) => {
    const rt = resource.resourceType as string;
    return {
      resource,
      request: { method: "POST", url: rt },
    };
  });
  return {
    resourceType: "Bundle",
    type: "transaction",
    entry: entries,
  };
}

export function chunkResources<T>(resources: T[], size: number): T[][] {
  const batch = size > 0 ? size : 50;
  const batches: T[][] = [];
  for (let i = 0; i < resources.length; i += batch) {
    batches.push(resources.slice(i, i + batch));
  }
  return batches;
}

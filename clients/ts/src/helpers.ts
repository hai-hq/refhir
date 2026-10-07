import type { Client } from "./client.js"; // type-only
import type { Args, JsonValue } from "./types.js";
import type { Watch } from "./watch.js";

export async function search(
  client: Client,
  resourceType: string,
  params: Record<string, string>,
): Promise<Watch> {
  const args: Args = { ...params };
  return client.watch(`${resourceType}/search`, args);
}

export async function read(
  client: Client,
  resourceType: string,
  id: string,
): Promise<Watch> {
  return client.watch(`${resourceType}/read`, { id });
}

export async function query(
  client: Client,
  path: string,
  args: Args = {},
): Promise<JsonValue> {
  const watch = await client.watch(path, args);
  try {
    const value = watch.latest();
    if (value === null) throw new Error("refhir: query returned no value");
    return value;
  } finally {
    await watch.close();
  }
}

export async function create(
  client: Client,
  resourceType: string,
  resource: JsonValue,
): Promise<JsonValue> {
  return client.mutate(`${resourceType}/create`, { resource });
}

export async function update(
  client: Client,
  resourceType: string,
  id: string,
  resource: JsonValue,
  ifMatch?: string,
): Promise<JsonValue> {
  const args: Args = { id, resource };
  if (ifMatch) args.ifMatch = ifMatch;
  return client.mutate(`${resourceType}/update`, args);
}

export async function deleteResource(
  client: Client,
  resourceType: string,
  id: string,
): Promise<JsonValue> {
  return client.mutate(`${resourceType}/delete`, { id });
}

export async function transaction(
  client: Client,
  bundle: JsonValue,
): Promise<JsonValue> {
  return client.mutate("Bundle/transaction", { resource: bundle });
}

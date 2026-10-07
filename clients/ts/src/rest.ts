import { HttpError } from "./errors.js";
import type { Client } from "./client.js";
import type { JsonValue } from "./types.js";

export async function metadata(client: Client): Promise<JsonValue> {
  const body = await doRequest(client, "GET", "/metadata");
  return JSON.parse(new TextDecoder().decode(body)) as JsonValue;
}

export async function doRequest(
  client: Client,
  method: string,
  path: string,
  body?: Uint8Array,
): Promise<Uint8Array> {
  const base = client.httpOrigin();
  const url = path.startsWith("/") ? `${base}${path}` : `${base}/${path}`;
  const headers: Record<string, string> = {
    Accept: "application/fhir+json",
  };
  if (body) headers["Content-Type"] = "application/fhir+json";
  const token = client.authToken();
  if (token) headers.Authorization = `Bearer ${token}`;

  const response = await fetch(url, {
    method,
    headers,
    body: toFetchBody(body),
  });
  const data = new Uint8Array(await response.arrayBuffer());
  if (response.status < 200 || response.status >= 300) {
    throw new HttpError(response.status, data);
  }
  return data;
}

function toFetchBody(body?: Uint8Array): BodyInit | undefined {
  if (!body) return undefined;
  const bytes = new Uint8Array(body);
  if (typeof Blob !== "undefined") {
    return new Blob([bytes]);
  }
  return Buffer.from(bytes);
}

import { HttpError } from "./errors.js";
import type { Client } from "./client.js";
import type { JsonValue } from "./types.js";

export async function metadata(client: Client): Promise<JsonValue> {
  const body = await doRequest(client, "GET", "/metadata");
  return JSON.parse(new TextDecoder().decode(body)) as JsonValue;
}

/** Response header carrying the commit (or read snapshot) timestamp. */
export const COMMIT_TS_HEADER = "X-Refhir-Commit-Ts";
/** Request header asking a replica to catch up to a timestamp before answering a read. */
export const MIN_TS_HEADER = "X-Refhir-Min-Ts";

/** Response body plus headers from one REST call. */
export type RestResponse = {
  status: number;
  headers: Headers;
  body: Uint8Array;
};

/**
 * One HTTP call, returning only the body. With read-your-writes on (the
 * default), the request carries `X-Refhir-Min-Ts` and the response's
 * `X-Refhir-Commit-Ts` feeds `client.lastCommitTs`.
 */
export async function doRequest(
  client: Client,
  method: string,
  path: string,
  body?: Uint8Array,
): Promise<Uint8Array> {
  return (await requestWithHeaders(client, method, path, body)).body;
}

/**
 * Like `doRequest` but also returns the status and response headers.
 * Non-2xx responses still throw `HttpError`. In browsers, `headers` only holds
 * what the server lists in `Access-Control-Expose-Headers`.
 */
export async function requestWithHeaders(
  client: Client,
  method: string,
  path: string,
  body?: Uint8Array,
): Promise<RestResponse> {
  const base = client.httpOrigin();
  const url = path.startsWith("/") ? `${base}${path}` : `${base}/${path}`;
  const headers: Record<string, string> = {
    Accept: "application/fhir+json",
  };
  if (body) headers["Content-Type"] = "application/fhir+json";
  const token = client.authToken();
  if (token) headers.Authorization = `Bearer ${token}`;
  const minTs = client.minTs();
  if (minTs !== undefined) headers[MIN_TS_HEADER] = minTs.toString();

  const response = await fetch(url, {
    method,
    headers,
    body: toFetchBody(body),
  });
  const data = new Uint8Array(await response.arrayBuffer());
  client.observeTs(response.headers.get(COMMIT_TS_HEADER));
  if (response.status < 200 || response.status >= 300) {
    throw new HttpError(response.status, data);
  }
  return { status: response.status, headers: response.headers, body: data };
}

function toFetchBody(body?: Uint8Array): BodyInit | undefined {
  if (!body) return undefined;
  const bytes = new Uint8Array(body);
  if (typeof Blob !== "undefined") {
    return new Blob([bytes]);
  }
  return Buffer.from(bytes);
}

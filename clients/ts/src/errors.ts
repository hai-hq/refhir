import type { JsonValue } from "./types.js";

export class OutcomeError extends Error {
  readonly outcome: JsonValue;

  constructor(outcome: JsonValue) {
    super(`refhir: ${JSON.stringify(outcome)}`);
    this.name = "OutcomeError";
    this.outcome = outcome;
  }
}

export class HttpError extends Error {
  readonly status: number;
  readonly body: Uint8Array;

  constructor(status: number, body: Uint8Array) {
    super(`refhir: HTTP ${status}`);
    this.name = "HttpError";
    this.status = status;
    this.body = body;
  }
}

/**
 * A `/sync` upgrade the server refused: it answered the WebSocket handshake with an HTTP error.
 * `dial` rejects with it, and so does a session that could not dial again. A 401 or 403 is final:
 * the session ends rather than dial a server that has said the token does not stand.
 *
 * Only Node can see this. A browser's WebSocket does not say why a handshake failed, so there the
 * client keeps retrying with backoff.
 */
export class UpgradeError extends HttpError {
  /** What the server's `Retry-After` header asked for, in milliseconds; 0 when it did not. */
  readonly retryAfterMs: number;

  constructor(status: number, retryAfterMs = 0) {
    super(status, new Uint8Array());
    this.name = "UpgradeError";
    this.message = `refhir: the server refused the /sync upgrade: HTTP ${status}`;
    this.retryAfterMs = retryAfterMs;
  }
}

/** The longest a `Retry-After` can hold a redial back. */
export const MAX_RETRY_AFTER_MS = 60_000;

/** Read a `Retry-After` header: a number of seconds, or an HTTP date. */
export function parseRetryAfter(value: string | undefined): number {
  const text = (value ?? "").trim();
  if (text === "") return 0;
  let ms = Number.NaN;
  if (/^-?\d+$/.test(text)) ms = Number.parseInt(text, 10) * 1000;
  else {
    const when = Date.parse(text);
    if (!Number.isNaN(when)) ms = when - Date.now();
  }
  if (Number.isNaN(ms) || ms < 0) return 0;
  return Math.min(ms, MAX_RETRY_AFTER_MS);
}

/**
 * A mutation failed because the socket went down while it was in flight. It may have been applied:
 * the answer was lost, not the write. Check before sending it again, since a create assigns a new
 * id each time.
 */
export class DisconnectedError extends Error {
  constructor() {
    super("refhir: disconnected");
    this.name = "DisconnectedError";
  }
}

/**
 * A mutation was not sent, because the client had no socket to send it on, as while it dials
 * again after the connection dropped (or because its `signal` had aborted). Nothing was written, so
 * it is safe to try again once `client.connected` is true.
 */
export class NotSentError extends Error {
  constructor() {
    super("refhir: disconnected: the mutation was not sent");
    this.name = "NotSentError";
  }
}

/**
 * A live query the server stopped answering: its first run failed, or a later run did and the
 * server dropped it. `watch.next()` rejects with it. Once a query has failed it is over; add it
 * again to watch again.
 *
 * `detail` is plain text whose wording is not a contract. `code` is: the FHIR IssueType the server
 * gave, such as `deleted` (the watched resource was deleted, the usual reason), `not-found`,
 * `forbidden` (scopes), `throttled` (a rate limit or quota), `too-costly` (a cap, such as the number
 * of live queries), `invalid`, `transient` or `exception`. It is empty for a server that sends none.
 */
export class QueryError extends Error {
  readonly path: string;
  readonly code: string;
  readonly detail: string;

  constructor(path: string, detail: string, code = "") {
    super(`refhir: ${path}: ${detail}`);
    this.name = "QueryError";
    this.path = path;
    this.code = code;
    this.detail = detail;
  }
}

export const errClosed = new Error("refhir: client closed");
export const errDisconnected = new DisconnectedError();
export const errNotSent = new NotSentError();

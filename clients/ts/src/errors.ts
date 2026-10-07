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

export const errClosed = new Error("refhir: client closed");
export const errDisconnected = new Error("refhir: disconnected");

/**
 * ReFHIR TypeScript client: `/sync` session for live queries and mutations.
 *
 * Node uses `ws` with Authorization on upgrade. The browser build uses native
 * WebSocket and `?access_token=` on `/sync`.
 */
export { Client } from "./client.js";
export type { CallOptions, DialOptions, TokenSource } from "./client.js";
export { attachAccessToken } from "./url.js";
export {
  OutcomeError,
  HttpError,
  UpgradeError,
  QueryError,
  NotSentError,
  DisconnectedError,
} from "./errors.js";
export { Watch } from "./watch.js";
export type { Args, JsonValue } from "./types.js";

export {
  search,
  read,
  query,
  create,
  update,
  deleteResource,
  transaction,
} from "./helpers.js";

export {
  metadata,
  doRequest,
  requestWithHeaders,
  COMMIT_TS_HEADER,
  MIN_TS_HEADER,
} from "./rest.js";
export type { RestResponse } from "./rest.js";

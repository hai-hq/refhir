/**
 * ReFHIR TypeScript client: `/sync` session for live queries and mutations.
 *
 * Node uses `ws` with Authorization on upgrade. The browser build uses native
 * WebSocket and `?access_token=` on `/sync`.
 */
export { Client } from "./client.js";
export type { DialOptions } from "./client.js";
export { attachAccessToken } from "./url.js";
export { OutcomeError, HttpError } from "./errors.js";
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

export { metadata, doRequest } from "./rest.js";

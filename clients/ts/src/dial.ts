import { Client, type DialOptions } from "./client.js";
import { isBrowser } from "./runtime.js";
import { openBrowserSocket } from "./transport-browser.js";
import { openNodeSocket } from "./transport-node.js";
import { splitBase } from "./url.js";
import type { SocketConnection } from "./socket.js";

export { isBrowser };

function openSocket(options: DialOptions): (wsURL: string) => Promise<SocketConnection> {
  const runtime = options.runtime ?? (isBrowser() ? "browser" : "node");
  const token = options.token;
  if (runtime === "browser") {
    return (wsURL) => openBrowserSocket(wsURL, token);
  }
  return (wsURL) => openNodeSocket(wsURL, token);
}

export async function dial(baseURL: string, options: DialOptions = {}): Promise<Client> {
  const { httpBase, wsURL } = splitBase(baseURL);
  return Client.connect(httpBase, wsURL, options.token, openSocket(options));
}

/** Explicit browser entry (e.g. bundlers that define `process`). */
export async function dialBrowser(
  baseURL: string,
  options: Omit<DialOptions, "runtime"> = {},
): Promise<Client> {
  return dial(baseURL, { ...options, runtime: "browser" });
}

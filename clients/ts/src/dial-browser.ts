import { Client, type DialOptions } from "./client.js";
import { isBrowser } from "./runtime.js";
import { openBrowserSocket } from "./transport-browser.js";
import { splitBase } from "./url.js";

export { isBrowser };

export async function dial(baseURL: string, options: DialOptions = {}): Promise<Client> {
  if (options.runtime === "node") {
    throw new Error("refhir: the browser build does not include the Node WebSocket transport");
  }
  const { httpBase, wsURL } = splitBase(baseURL);
  return Client.connect(
    httpBase,
    wsURL,
    options.token,
    (url) => openBrowserSocket(url, options.token),
    { readYourWrites: options.readYourWrites },
  );
}

export async function dialBrowser(
  baseURL: string,
  options: Omit<DialOptions, "runtime"> = {},
): Promise<Client> {
  return dial(baseURL, { ...options, runtime: "browser" });
}

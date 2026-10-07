export function splitBase(baseURL: string): { httpBase: string; wsURL: string } {
  const parsed = new URL(baseURL);
  const scheme = parsed.protocol.replace(":", "");
  if (!["http", "https", "ws", "wss"].includes(scheme)) {
    throw new Error(`refhir: unsupported scheme ${scheme}`);
  }

  const ws = new URL(parsed.toString());
  if (ws.protocol === "http:") ws.protocol = "ws:";
  else if (ws.protocol === "https:") ws.protocol = "wss:";

  let path = ws.pathname.replace(/\/$/, "");
  if (!path.endsWith("/sync")) {
    path = path === "" ? "/sync" : `${path}/sync`;
  }
  ws.pathname = path;
  ws.search = "";
  ws.hash = "";

  const http = new URL(ws.toString());
  if (http.protocol === "ws:") http.protocol = "http:";
  else if (http.protocol === "wss:") http.protocol = "https:";
  http.pathname = http.pathname.replace(/\/sync$/, "") || "/";

  const httpBase = http.toString().replace(/\/$/, "");
  return { httpBase, wsURL: ws.toString() };
}

/** Browser WebSockets cannot set Authorization; use ?access_token= on /sync. */
export function attachAccessToken(wsURL: string, token?: string): string {
  if (!token) return wsURL;
  const url = new URL(wsURL);
  url.searchParams.set("access_token", token);
  return url.toString();
}

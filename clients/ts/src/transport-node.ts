import WebSocket from "ws";

import { parseRetryAfter, UpgradeError } from "./errors.js";
import type { SocketConnection } from "./socket.js";

export async function openNodeSocket(
  wsURL: string,
  token?: string,
): Promise<SocketConnection> {
  const headers: Record<string, string> = {};
  if (token) headers.Authorization = `Bearer ${token}`;
  const socket = new WebSocket(wsURL, { headers });
  await new Promise<void>((resolve, reject) => {
    socket.once("open", () => resolve());
    // The server answered the handshake with an HTTP error: say which, so the client can tell a
    // token that will never work (401) from a server that is busy (429) or down.
    socket.once("unexpected-response", (req, res) => {
      const header = res.headers["retry-after"];
      const retryAfter = parseRetryAfter(Array.isArray(header) ? header[0] : header);
      reject(new UpgradeError(res.statusCode ?? 0, retryAfter));
      req.destroy();
    });
    socket.once("error", (err) => reject(err));
  });
  return wrapNode(socket);
}

function wrapNode(socket: WebSocket): SocketConnection {
  let onMessage: ((text: string) => void) | null = null;
  let onClose: (() => void) | null = null;
  socket.on("message", (data) => onMessage?.(data.toString()));
  socket.on("close", () => onClose?.());
  return {
    get readyState() {
      return socket.readyState;
    },
    send(text: string) {
      socket.send(text);
    },
    close() {
      socket.close();
    },
    terminate() {
      socket.terminate();
    },
    onMessage(handler) {
      onMessage = handler;
    },
    onClose(handler) {
      onClose = handler;
    },
    removeListeners() {
      socket.removeAllListeners();
      onMessage = null;
      onClose = null;
    },
  };
}

import type { SocketConnection } from "./socket.js";
import { attachAccessToken } from "./url.js";

export async function openBrowserSocket(
  wsURL: string,
  token?: string,
): Promise<SocketConnection> {
  const url = attachAccessToken(wsURL, token);
  const socket = new WebSocket(url);
  await new Promise<void>((resolve, reject) => {
    socket.addEventListener("open", () => resolve(), { once: true });
    socket.addEventListener(
      "error",
      () => reject(new Error("refhir: websocket failed to open")),
      { once: true },
    );
  });
  return wrapBrowser(socket);
}

function wrapBrowser(socket: WebSocket): SocketConnection {
  let onMessage: ((text: string) => void) | null = null;
  let onClose: (() => void) | null = null;
  socket.addEventListener("message", (ev) => {
    if (typeof ev.data === "string") onMessage?.(ev.data);
    else onMessage?.(new TextDecoder().decode(ev.data as ArrayBuffer));
  });
  socket.addEventListener("close", () => onClose?.());
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
      socket.close();
    },
    onMessage(handler) {
      onMessage = handler;
    },
    onClose(handler) {
      onClose = handler;
    },
    removeListeners() {
      onMessage = null;
      onClose = null;
    },
  };
}

export const SOCKET_OPEN = 1;

export interface SocketConnection {
  send(text: string): void;
  close(): void;
  terminate(): void;
  readonly readyState: number;
  onMessage(handler: (text: string) => void): void;
  onClose(handler: () => void): void;
  removeListeners(): void;
}

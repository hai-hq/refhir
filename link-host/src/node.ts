import type { Server } from 'node:http';

import { serve } from '@hono/node-server';

import type { Host } from './app.js';

export interface ListenOptions {
  port: number;
  hostname?: string;
  /** How long a connection may take to send its headers (default 10) and the whole request (120). */
  headersTimeoutSeconds?: number;
  requestTimeoutSeconds?: number;
}

/** Serves a host from Node, and answers when it is listening. */
export function listen(host: Host, options: ListenOptions): Promise<Server> {
  const headersTimeout = (options.headersTimeoutSeconds ?? 10) * 1000;
  return new Promise((resolve, reject) => {
    const server = serve(
      {
        fetch: host.app.fetch,
        port: options.port,
        hostname: options.hostname ?? '127.0.0.1',
        // Node looks for connections that are too slow every `connectionsCheckingInterval`: often enough
        // that the timeouts below are kept to within a few seconds of their word.
        serverOptions: { maxHeaderSize: 8192, connectionsCheckingInterval: Math.max(250, Math.min(5000, headersTimeout / 2)) },
      },
      () => resolve(server),
    ) as Server;
    server.headersTimeout = headersTimeout;
    server.requestTimeout = (options.requestTimeoutSeconds ?? 120) * 1000;
    server.keepAliveTimeout = 5000;
    server.once('error', reject);
  });
}

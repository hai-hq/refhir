#!/usr/bin/env node
// Start a link host from the environment. See the README for the settings.
import { createHost } from './host.mjs';

const { ADMIN_TOKEN, PORT, PUBLIC_URL, DATA_DIR, TICKET_SECRET } = process.env;
if (!ADMIN_TOKEN || ADMIN_TOKEN.length < 16) {
  console.error('Set ADMIN_TOKEN to a secret of 16 characters or more: it is what lets a caller make links.');
  process.exit(1);
}
const port = Number(PORT ?? 8090);
const server = createHost({
  adminToken: ADMIN_TOKEN,
  dataDir: DATA_DIR ?? 'shl-data',
  publicUrl: PUBLIC_URL,
  ticketSecret: TICKET_SECRET,
});
server.listen(port, () => console.log(`link host listening on port ${port}`));
// A container is stopped with SIGTERM: finish what is being answered, then go.
for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => server.close(() => process.exit(0)));

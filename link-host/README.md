# SMART Health Links host

A host for [SMART Health Links](https://hl7.org/fhir/uv/smart-health-cards-and-links/links-specification.html).
It stores encrypted files and answers whoever opens a link. It does not belong to ReFHIR and does
not talk to it: a sender app fetches a patient's summary from ReFHIR, encrypts it, and puts the
encrypted file here. See [SMART Health Links](../docs/health-links.md) for how the pieces fit.

The host never has a key. A file arrives encrypted, is kept as it came, and is handed out as it
is, so a host that is broken into holds nothing that can be read.

It is a [Hono](https://hono.dev) server written in TypeScript, and keeps its links in Postgres, or in a
SQLite file when it is one machine and wants no database server. It is meant to be left running: it
limits how often it may be asked, clears away what has ended, logs what happened without logging
anything that opens a link, says whether it is ready, and stops without dropping what it is
answering. Several copies can run against one database.

## Start it

### With Docker Compose, and HTTPS that looks after itself

This starts the host with [Postgres](https://www.postgresql.org) behind it and
[Caddy](https://caddyserver.com) in front, which gets and renews a certificate for your domain.

1. Point a DNS name at the machine and open ports 80 and 443.
2. In the `deploy` directory, make a `.env` file:

   ```bash
   DOMAIN=links.example.org
   ADMIN_TOKEN=<a secret: openssl rand -base64 32>
   POSTGRES_PASSWORD=<another: openssl rand -hex 32>
   ```

3. Start it:

   ```bash
   docker compose up -d
   ```

The links and their files are in the `pg-data` volume. Back up the database.

### With Docker Compose, on one machine, with no database server

`deploy/compose.sqlite.yaml` is the same, with the host keeping its links in a SQLite file in a
volume, and no Postgres. It needs `DOMAIN` and `ADMIN_TOKEN` in the `.env` file, and no password:

```bash
docker compose -f compose.sqlite.yaml up -d
```

See [Choosing a database](#choosing-a-database) for what that is and is not good for.

### As a container

```bash
docker build -t refhir-link-host .
docker run -d -p 8090:8090 \
  -e DATABASE_URL=postgres://user:password@db.example:5432/links \
  -e ADMIN_TOKEN=<a secret of 16 characters or more> \
  -e PUBLIC_URL=https://links.example.org \
  -e TRUST_PROXY=1 \
  refhir-link-host
```

The container keeps nothing on its own disk, so it can be replaced, scaled and restarted freely. The
schema is made on first start. With SQLite it is the same, with `-v links:/data` and
`-e DATABASE_URL=sqlite:///data/links.db` in place of the Postgres address.

Put something in front of it that serves HTTPS, and set `PUBLIC_URL` to the address it is reached
at. `TRUST_PROXY=1` says there is one proxy in front, so the host counts callers by the address the
proxy saw and not by the proxy's own.

### From a checkout

To try it on your own machine:

```bash
npm ci && npm run build
ADMIN_TOKEN=<a secret of 16 characters or more> HOST=127.0.0.1 DATABASE_URL=sqlite:links.db node dist/server.js
```

It listens on `http://localhost:8090` and makes links for that address, keeping them in `links.db`.
With `STORE=memory` in place of `DATABASE_URL` they are kept in memory and lost when it stops.
`node dist/server.js --check` reads the settings, makes sure the database can be written, and stops,
which is a good last step of a deploy.

## Make a link

The sender, which holds an `ADMIN_TOKEN` (or the token of one of the [clients](#who-may-make-links)),
makes a link, puts the encrypted file on it, and builds the `shlink:/` address from the manifest URL
and its own key. `docs/examples/share-summary.mjs` in the ReFHIR repository does all of it.

| Request | What it does |
|---|---|
| `POST /shl` | Makes a link. Needs `Authorization: Bearer <token>`. The optional JSON body has `passcode` (up to 64 characters), `attempts` (how many wrong passcodes, 1 to 20, default 5) and `expiresAt` (seconds since 1970). Answers `{id, manifestUrl, manageToken, expiresAt, passcodeRequired}`. Without `expiresAt` a link lasts 30 days. Put `expiresAt` in the link's own `exp`. |
| `POST /shl/{id}/file?contentType=...` | Puts the encrypted file on the link. Needs the link's `manageToken`. The body is the file, a compact JWE. The `contentType` is what the manifest will say it is, `application/fhir+json` unless you name another. |
| `GET /shl/{id}/access` | Who has asked, and when. Needs the `manageToken`. Answers `{access, expiresAt, active}`. Kept for 30 days after the link ends. |
| `DELETE /shl/{id}` | Switches the link off, and deletes its files at once. Needs the `manageToken`. |

The request a receiver makes needs nothing but the link:

| Request | What it does |
|---|---|
| `POST /shl/{id}` | The manifest. A JSON body of `recipient`, and `passcode` and `embeddedLengthMax` when they apply. Answers `{files: [...]}`: each file's `contentType` and a `location` to fetch it from, or the file itself in `embedded` when it fits in the `embeddedLengthMax` the receiver gave. |
| `GET /shl/{id}/file/{file}?ticket=...` | One file, to whoever holds the ticket the manifest gave. A ticket lasts a minute. |
| `GET /shl/{id}?recipient=...` | The file itself, for a link with the `U` flag: no passcode, and one file. |

And the ones for whoever runs the host:

| Request | What it does |
|---|---|
| `GET /healthz` | `200` when it is up. |
| `GET /readyz` | `200` when it can take work: it can write to its database and is not stopping. `503` otherwise. Point a balancer at this one. |
| `GET /metrics` | Counts, in the Prometheus text format. Needs `Authorization: Bearer <METRICS_TOKEN>`, and is `404` when there is no `METRICS_TOKEN`. |

Each entry of an access list says who asked as `recipient` (whatever the caller wrote), `at`, the
`outcome` (`opened` or `wrong-passcode`) and, unless `RECORD_CLIENT_INFO` is `false`, the `ip` and
`userAgent` the request came from.

Answers an operator will meet: `400` for a request that cannot be read, `401` for a token or a
passcode that is wrong (a passcode refusal says `remainingAttempts`), `403` for an address or a hook
that refuses, `404` for a link that has expired, been switched off or never existed (the three look
alike), `409` for a link that is full, `413` for a body that is too large, `429` for too many requests
and `503` for a host that is busy or stopping. The last two say in `Retry-After` how long to wait.

## Settings

The command is set by environment variables. A mistake is reported with every other one, and the host
does not start.

### What it needs

| Setting | |
|---|---|
| `DATABASE_URL` | Where links are kept. A Postgres, `postgres://user:password@host:5432/database` (add `?sslmode=require` for one that insists on TLS), or a SQLite file, `sqlite:///data/links.db` for an absolute path and `sqlite:links.db` for one from where the host is run. The kind of address says which. Required, unless `STORE=memory`. |
| `ADMIN_TOKEN` | A secret of 16 characters or more. It lets a caller make links. Either this or `CLIENTS_FILE` is required. |
| `CLIENTS_FILE` | A JSON file of [clients](#who-may-make-links), for a host that has more than one sender. |
| `PUBLIC_URL` | The address the host is reached at, which the links it makes point to. It has to be `https`, except for `localhost`. Required, unless `HOST` is `127.0.0.1`, `::1` or `localhost`, when it is that address. |

### Network

| Setting | |
|---|---|
| `HOST` | The address to listen on. Default `0.0.0.0`. |
| `PORT` | The port to listen on. Default `8090`. |
| `TRUST_PROXY` | How many proxies are in front, so that the host counts callers by their address and not the proxy's. Default `0`. The host takes the address that many places from the right of `X-Forwarded-For`, which is the part each proxy writes. It says so when `PUBLIC_URL` is `https` and this is `0`. |
| `CORS_ORIGINS` | The pages that may call what a receiver calls, comma-separated. Default `*`, so a viewer in a browser works anywhere. |
| `MANAGEMENT_CORS_ORIGINS` | The pages that may call what the owner of a link calls. Default none: a browser on another site cannot make a link. |
| `MANAGEMENT_ALLOW` | Addresses and ranges, comma-separated (`203.0.113.10, 10.0.0.0/8`), the only ones from which links may be made, filled, read about or switched off. Default any. |
| `HEADERS_TIMEOUT_SECONDS`, `REQUEST_TIMEOUT_SECONDS` | How long a connection may take to send its headers, and the whole request. Default 10 and 120. |

The host does not serve HTTPS itself. Something in front of it does, which is Caddy in the compose
file, and is a load balancer, an ingress or the platform's own router elsewhere.

### The database

| Setting | |
|---|---|
| `AUTO_MIGRATE` | Make and update the schema when the host starts. Default `true`. Processes that start together take turns, so it is run once. With `false` the host will not start on a database that has no schema, and `refhir-link-host migrate` makes it. |
| `DATABASE_POOL_MAX` | Postgres: the most connections to open. Default 10. |
| `DATABASE_WAIT_SECONDS` | Postgres: how long to wait for the database to be there when the host starts, as it is when both come up together. Default 30. |
| `STORE` | `memory` keeps links in memory, which a restart loses: for trying the host out. The other two, `postgres` and `sqlite`, need not be said, and are an error if they are not the kind of `DATABASE_URL`. |
| `TICKET_SECRET` | Signs the one-minute tickets that give out files. Default: made once and kept in the database, so that every copy of the host agrees on it. |

### Links and files

| Setting | |
|---|---|
| `DEFAULT_LIFETIME_DAYS`, `MAX_LIFETIME_DAYS` | How long a link lasts when it is not told, and the longest it may be asked to. Default 30 and 365. |
| `DEFAULT_ATTEMPTS`, `MAX_ATTEMPTS` | Wrong passcodes a link allows, and the most a sender may ask for. Default 5 and 20. |
| `MAX_FILE_BYTES`, `MAX_FILES_PER_LINK` | The largest file, and the most files on a link. Default 10 MB and 10. |
| `REQUIRE_PASSCODE` | `true` makes every link need a passcode. |
| `ALLOWED_CONTENT_TYPES` | The media types of file the host keeps, comma-separated, such as `application/fhir+json,application/smart-health-card`. Default any. |
| `RETENTION_DAYS` | How long a record is kept after its link ends, so that its owner can still read who asked. Default 30. The encrypted files go when the link ends. |
| `SWEEP_INTERVAL_MINUTES` | How often it clears away what has ended. Default 10. `0` turns it off. Only one copy of the host does it at a time. |

### Limits

Each is a number of requests, `0` allows any number, and all are counted by each copy of the host on
its own.

| Setting | |
|---|---|
| `RATE_LIMIT_PUBLIC` | What a receiver may ask in a minute, from one address. Default 120. |
| `RATE_LIMIT_PER_LINK` | What anyone may ask of one link in a minute. Default 60. This bounds the work of checking passcodes. |
| `RATE_LIMIT_MANAGEMENT` | What an owner may ask in a minute, from one address. Default 300. |
| `RATE_LIMIT_CREATE` | The links one client may make in a minute. Default 60. |
| `RATE_LIMIT_AUTH_FAILURES` | Wrong tokens from one address in five minutes before it is shut out of what needs a token, for the rest of the five minutes, even with the right one. Default 10. |
| `MAX_CONCURRENT` | Requests at once. More are told to come back. Default 200. |

### Logging, counts and stopping

| Setting | |
|---|---|
| `LOG_REQUESTS` | `false` stops the line for each request. Events (a link made, ended, refused) are always logged. |
| `RECORD_CLIENT_INFO` | `false` keeps the address and the browser of whoever asks out of the access list. Default `true`. |
| `METRICS_TOKEN` | Turns on `/metrics`. |
| `SHUTDOWN_DELAY_SECONDS` | After `SIGTERM`, how long to go on answering, saying it is not ready, before it stops listening, so that a balancer has seen. Default 0. |
| `SHUTDOWN_GRACE_SECONDS` | Then how long to wait for what is being answered. Default 10. |

`refhir-link-host --help` lists the commands. `refhir-link-host new-token` makes a token and the
hash of it for a clients file, `migrate` brings the schema up to date, and `backup <file>` copies a
SQLite database (see below).

## Choosing a database

| | Postgres | SQLite |
|---|---|---|
| Good for | A host you run for others, on more than one machine, or one that may grow | One machine, no database server, the least to run |
| Copies of the host | Any number, on any machines | Several on one machine, such as the new one and the old during a deploy |
| A guess at a passcode | Counted in a transaction that holds the link's row | Counted once between copies: a change that finds the link changed under it is run again |
| Where it lives | The database server | A file, and the `-wal` and `-shm` files beside it, on a disk of the machine |
| Back it up with | `pg_dump`, or whatever backs up your other databases | `refhir-link-host backup <file>` |

A few things to know before choosing SQLite:

- **One machine, a disk of its own.** SQLite cannot lock a file on a network disk (NFS, SMB) safely, so
  the file has to be on the machine's own disk or a volume attached to it.
- **It is Node's own SQLite**, the built-in `node:sqlite`, which needs Node 22.13 or later and which
  Node still calls experimental. Nothing is installed with it. Node's note about that is left out of
  the host's output, which is JSON, and is this one.
- **A write holds up the whole process while it is made**, which is short, and while it waits for a
  write that another copy is making, for at most five seconds, after which the caller is told to
  try again. It is one writer at a time, so it is not for a great many writes at once. A link host is
  not that: a link is written when it is made, filled, opened and ended.
- **Do not copy the database file to back it up** while the host is running: some of what it holds is in
  the `-wal` file beside it. `refhir-link-host backup /data/backup.db` writes a copy that is the whole
  database at one moment, while the host goes on writing, and a host can start on it. Under Compose:
  `docker compose -f compose.sqlite.yaml exec link-host node dist/server.js backup /data/backup.db`,
  then `docker compose cp link-host:/data/backup.db .`.

Moving from one to the other is not done for you: links are short-lived, so the usual way is to start
the new host beside the old, make new links there, and switch the old off when its links have ended.

## Who may make links

One `ADMIN_TOKEN` is enough for one sender. A host with several, or that wants each held to what it
needs, has a clients file, a JSON file named by `CLIENTS_FILE`:

```json
{
  "clients": [
    {
      "name": "ehr",
      "tokenHash": "sha256:5d41402abc4b2a76b9719d911017c592...",
      "allow": ["203.0.113.10"],
      "maxLifetimeDays": 90,
      "defaultLifetimeDays": 30,
      "requirePasscode": true,
      "maxFilesPerLink": 2
    },
    { "name": "patient-portal", "token": "a secret of 16 characters or more" }
  ]
}
```

- `name` is what the logs and each link say made it.
- Give `token`, or `tokenHash`, which is `sha256:` and the SHA-256 of the token in hex.
  `refhir-link-host new-token` prints a token and its hash: give the client the token, keep the hash.
  The host checks a token against its hash and does not keep it.
- `allow` ties the client to its own addresses. A right token from anywhere else is a `403`.
- The rest are the limits of `DEFAULT_LIFETIME_DAYS`, `MAX_LIFETIME_DAYS`, `DEFAULT_ATTEMPTS`,
  `MAX_ATTEMPTS`, `MAX_FILE_BYTES`, `MAX_FILES_PER_LINK` and `REQUIRE_PASSCODE`, for this client alone.
  A setting that is misspelt is an error and not ignored.

Revoking a client is taking it out of the file and restarting the host. Links it made keep working
until they end, and their owners keep their own tokens.

## What it protects, and what it does not

- **The data.** A host holds ciphertext and never the key, which is only in the link. A host that is
  taken over can delete links or hand out garbage, which a receiver's decryption rejects, and cannot
  read anything.
- **The link.** Its address is 256 random bits, a passcode is stored hashed and counted, and a link
  that has expired, been switched off or never existed answers alike. The files go as soon as it ends.
  A guess at a passcode is counted in a transaction that holds the link's row, so it is counted once
  however many copies of the host receive guesses together.
- **The tokens.** Compared in constant time, tried against every client alike, and a caller who keeps
  getting one wrong is shut out. Nothing that opens a link is ever logged: a link is named in a log
  by its first eight characters.
- **The host.** Requests, headers, bodies, time and concurrency are all limited, every answer says not
  to be cached, and a file that is not encrypted is refused.

Things it cannot do, which are for the sender and the deployment:

- **Who is asking.** A recipient is whatever the caller wrote, which the specification does not let a
  host check. The access list adds the address and the browser, and a passcode sent another way is
  what keeps the wrong person out. A deployment that does know its recipients can check them
  ([hooks](#change-what-it-does)).
- **Whoever has the link.** A link is a key. Give it a short life, a passcode when it will pass
  through places you do not control, and switch it off when it is done.
- **What the host can see.** It learns that a link was opened, when, how large the file is and the
  address that asked, though not what was in it.

## Run it for real

- **Serve it over HTTPS**, from Caddy as above or any proxy. A receiver will not open a link to an
  address that is not `https`.
- **Say how many proxies there are** with `TRUST_PROXY`. Without it every caller looks like the
  proxy, and one noisy caller uses up the limit of all.
- **Back up the database**, with `pg_dump` or `refhir-link-host backup`. It holds each link's settings
  and its encrypted files, and no key and nothing readable.
- **Run several copies**, and replace them whenever you like: a new one may start before the old one
  stops, and they share the one database. With Postgres they can be on any machines; with SQLite, on
  the one. Limits are counted by each copy, so a caller may ask each copy its share.
- **Keep the database apart from the FHIR server's:** a database, a user and a password of its own, or
  a SQLite file the FHIR server cannot reach. A host that holds only ciphertext is a much smaller thing
  to lose.
- **Watch it.** Point the balancer at `/readyz`. Read the log, which is a JSON line for each thing
  that happened. Scrape `/metrics`:

  | Series | |
  |---|---|
  | `shl_requests_total{route,status}` | Requests, by what they asked for. |
  | `shl_links_created_total`, `shl_files_stored_total`, `shl_files_served_total` | Work done. |
  | `shl_manifest_total{outcome}` | `opened`, `wrong_passcode`, `passcode_required`, `denied`, `not_found`. A rise in `wrong_passcode` is someone guessing. |
  | `shl_links_ended_total{reason}` | Links that ended by `attempts` or `revoked`. |
  | `shl_rate_limited_total{bucket}`, `shl_refused_total{reason}` | Requests that were turned away, and why. |
  | `shl_links_live`, `shl_stored_bytes` | What is kept, as of the last sweep. |
  | `shl_in_flight` | Requests being answered. |

- **Plan the database.** A link's file is at most `MAX_FILE_BYTES`, and a sender can make
  `RATE_LIMIT_CREATE` links a minute, so a token that leaks can fill a disk. Keep `RETENTION_DAYS` and
  the lifetimes short enough that it clears, and watch `shl_stored_bytes`.

## Change what it does

The command is a thin shell around a library, which is what to use when a deployment needs more than
settings:

```ts
import { createHost, listen, postgresStore, sqliteStore } from '@haihq/refhir-link-host';

const store = process.env.DATABASE_URL
  ? await postgresStore({ url: process.env.DATABASE_URL })
  : await sqliteStore({ path: './links.db' });
const host = await createHost({
  store,
  adminToken,
  publicUrl: 'https://links.example.org',
  trustProxy: 1,
  // Only people the directory knows may open a link.
  authorizeManifest: async ({ recipient, request }) => directory.knows(request.headers.get('x-viewer-token')),
  onEvent: (event) => audit.write(event),
});
await listen(host, { port: 8090, hostname: '0.0.0.0' });
```

`createHost` returns a `Host`: its `app` is a [Hono](https://hono.dev) app, so `host.app.fetch` is a
function from a web `Request` to a `Response`. Node is what it is tested on and what `listen` uses;
it needs Node's `crypto` and `net` modules, and a `remoteAddress` option where there is no Node socket
to read the caller's address from. It has `sweep()`, `drain()` and `store` besides, and the whole
app can be mounted inside a larger Hono app with `parent.route('/', host.app)`.

Most settings above are an option of the same name in camel case (`maxFileBytes`, `trustProxy`,
`corsOrigins`, `requirePasscode`). The ones that are not: lifetimes and times are in seconds
(`defaultLifetimeSeconds`, `maxLifetimeSeconds`, `retainSeconds`, `sweepIntervalSeconds`), `clients`
is the list itself and not a file, `rateLimits` is an object of `public`, `perLink`, `management`,
`create` and `authFailures`, and the database, logging and stopping belong to the command. Besides
those:

| Option | |
|---|---|
| `authenticate(request, { ip })` | Says who is making a link, by returning a client (`{ name, ...limits }`) or `null`, in place of the tokens. Use it to take a token from your own identity provider. |
| `authorizeCreate({ client, options, ip, request })` | Returns `false`, or `{ allow: false, status, error }`, to refuse a link before it is made. |
| `authorizeManifest({ link, recipient, ip, request })` | The same, for a receiver, before a passcode is looked at and so without using an attempt up. `link` says who made it and when it ends, and nothing that opens it. A hook that throws refuses. This is the place to require that a recipient is who the directory says. |
| `onEvent(event)` | Told of `link.created`, `file.stored`, `manifest.opened`, `manifest.denied`, `file.served`, `link.ended` and `client.refused`, each with `type`, `at` and the whole link `id`, which a log never has. Use it for an audit trail, or to tell the owner their link was opened. A failure here does not fail the request. |
| `logger(entry)` | Gets each log line as an object. Without one the library logs only errors. |
| `store` | Where links are kept: `postgresStore({ url })`, `sqliteStore({ path })`, or `memoryStore()` for trying out. Required. |
| `remoteAddress(c)` | The address a request came from, where it is not a Node request. |
| `now()` | The time in milliseconds, for tests. |

A store is an object of a few methods, listed with what each promises in
[`src/types.ts`](src/types.ts): keep a link, read it, change it in a transaction on that link, keep
a file, and clear away what has ended. The Postgres one is [`src/store/postgres.ts`](src/store/postgres.ts)
and the SQLite one [`src/store/sqlite.ts`](src/store/sqlite.ts); the schemas are in
[`migrations`](migrations). Any database that can do an update in a transaction can be one, and one that
cannot hold a link while a change runs, as SQLite cannot, can check at the end that the link is as the
change saw it, and run the change again if it is not.

## License

Apache-2.0, like the rest of ReFHIR.

# SMART Health Links host

A host for [SMART Health Links](https://hl7.org/fhir/uv/smart-health-cards-and-links/links-specification.html).
It stores encrypted files and answers whoever opens a link. It does not belong to ReFHIR and does
not talk to it: a sender app fetches a patient's summary from ReFHIR, encrypts it, and puts the
encrypted file here. See [SMART Health Links](../docs/health-links.md) for how the pieces fit.

The host never has a key. A file arrives encrypted, is kept as it came, and is handed out as it
is, so a host that is broken into holds nothing that can be read. It is one file of JavaScript
that needs Node 20 or later and no packages, small enough to read before you trust it with
anything.

## Run it

From a checkout, in this directory:

```bash
ADMIN_TOKEN=<a secret of 16 characters or more> \
PUBLIC_URL=https://links.example.org \
DATA_DIR=/var/lib/shl \
node src/server.mjs
```

As a container:

```bash
docker build -t refhir-link-host .
docker run -d -p 8090:8090 -v shl-data:/data \
  -e ADMIN_TOKEN=<a secret of 16 characters or more> \
  -e PUBLIC_URL=https://links.example.org \
  refhir-link-host
```

| Setting | |
|---|---|
| `ADMIN_TOKEN` | Required, 16 characters or more. It is what lets a caller make links. |
| `PUBLIC_URL` | The address the host is reached at, which the links it makes point to. Without it the `Host` header of each request is used, which is for trying the host out. |
| `PORT` | The port to listen on. Default `8090`. |
| `DATA_DIR` | Where links and their files are kept. Default `shl-data`, and `/data` in the container. |
| `TICKET_SECRET` | Signs the one-minute tickets that give out files. Made when the host starts if unset, so a restart ends the tickets then outstanding. |

## What it answers

| Request | What it does |
|---|---|
| `POST /shl` | Makes a link. Needs `Authorization: Bearer <ADMIN_TOKEN>`. The optional JSON body has `passcode`, `attempts` (1 to 20, default 5) and `expiresAt` (seconds since 1970, in the future). Answers `{id, manifestUrl, manageToken, passcodeRequired}`. |
| `POST /shl/{id}/file?contentType=...` | Puts the encrypted file on the link. Needs the link's `manageToken`. The body is the file, a compact JWE. The `contentType` is what the manifest will say it is, `application/fhir+json` unless you name another. |
| `POST /shl/{id}` | The manifest, for whoever opens the link: a JSON body of `recipient`, and `passcode` and `embeddedLengthMax` when they apply. Answers `{files: [...]}`. |
| `GET /shl/{id}/file/{file}?ticket=...` | One file, to whoever holds the ticket the manifest gave. A ticket lasts a minute. |
| `GET /shl/{id}?recipient=...` | The file itself, for a link with the `U` flag: no passcode, and one file. |
| `GET /shl/{id}/access` | Who has asked, and when. Needs the `manageToken`. |
| `DELETE /shl/{id}` | Switches the link off, and deletes its files. Needs the `manageToken`. |
| `GET /healthz` | `200` when it is up. |

What it does, which is what the specification asks of a host:

- A manifest has a `contentType` and a `location` for each file, or the file itself in `embedded`
  when it fits in the `embeddedLengthMax` the receiver gave.
- A link with a passcode counts the wrong ones. A wrong passcode is a `401` with
  `remainingAttempts`, and when they run out the link is switched off. A request with no passcode
  is not a guess, and does not use an attempt up. Guesses that arrive together are each counted.
- A link that has expired, been switched off or never existed answers `404`, and the three cannot be
  told apart.
- A file is stored exactly as it was sent, and only if it is a JWE of the kind the specification
  asks of a file (`alg` `dir`, `enc` `A256GCM`). A file that is not encrypted is refused, so one
  cannot be stored by mistake.
- A link's address is 256 bits, passcodes are stored hashed, and any origin may call the host, so a
  viewer in a browser can use it.

## Deploying it

- **Serve it over HTTPS**, from a reverse proxy, and set `PUBLIC_URL` to the address clients reach.
  A manifest tells the receiver where to fetch each file, and that address comes from `PUBLIC_URL`.
- **Run one process for each data directory.** A link's attempts are counted by taking turns inside
  the process, so two processes on one directory could each count the same guess once.
- **Back up `DATA_DIR`.** It holds each link's settings and its encrypted files. It holds no key and
  nothing readable.
- **Keep the host apart from the FHIR server:** another address, another credential, and no access
  to the database.
- **Keep `POST /shl` away from callers you do not trust.** One `ADMIN_TOKEN` is the only way to make
  a link, and a link can be given files up to 10 MB. There is no limit on requests beyond the
  attempts a passcode link allows, so rate-limit the manifest address at the proxy.

## As a library

```js
import { createHost } from '@haihq/refhir-link-host';

const server = createHost({ adminToken, dataDir: './shl-data', publicUrl: 'https://links.example.org' });
server.listen(8090);
```

`createHost` returns a Node `http.Server`. Besides the settings above it takes `maxFileBytes` (the
largest file, 10 MB by default) and `now`, a function that returns the time in milliseconds.

## License

Apache-2.0, like the rest of ReFHIR.

# SMART Health Links

A [SMART Health Link](https://hl7.org/fhir/uv/smart-health-cards-and-links/links-specification.html)
shares a patient's data without handing it over. The link is an address and a key, small enough for
a QR code. The data stays on a server, encrypted, and whoever opens the link fetches it and
decrypts it on their own device. Because the data is behind an address, it can be a whole
[patient summary](patient-summary.md), and it can expire, ask for a passcode, or be switched off.

ReFHIR is where the data comes from. It does not host links: a host holds the encrypted files and
answers the people who open the links, and ReFHIR holds the patient's record. They are kept apart
on purpose. A link's protection is that the key is in the link and nowhere else, so a host that
only ever holds ciphertext cannot expose a record even if it is broken into, and a server that
holds the clinical record does not have to answer anyone who holds a link.

This guide sets up a host, a sender and a link. The repository has a host to run, the
[link host](../link-host/README.md), and a sender, [`share-summary.mjs`](examples/share-summary.mjs).
Both are plain JavaScript that needs Node 20 or later and no packages.

## How the pieces fit

```
patient app ──(SMART token)──▶ ReFHIR  Patient/p1/$summary   the data
     │
     │  encrypts it with a new key; the key goes in the link and nowhere else
     ▼
 link host   ◀── holds only the encrypted file, and answers whoever opens the link
     ▲
     │  fetches the encrypted file, decrypts it with the key from the link
receiver (a clinician's app, a viewer)
```

1. **The sender** is an app the patient uses. It signs in with SMART, asks ReFHIR for the summary
   with the patient's token, makes a new 256-bit key, and encrypts the summary with it.
2. **The host** is given the encrypted file and gives the sender a link address (the manifest URL).
   It never sees the key.
3. **The link** is `shlink:/` and a base64url JSON payload with the manifest URL, the key, an
   expiry and a label. The sender shows it as a QR code or sends it.
4. **The receiver** opens the link, asks the host's manifest URL for the list of files, fetches the
   encrypted file, and decrypts it with the key from the link.

## Run a host

The [link host](../link-host/README.md) stores the encrypted files and answers whoever opens a link.
Run it from a checkout, in the `link-host` directory:

```bash
ADMIN_TOKEN=<a secret of 16 characters or more> \
PUBLIC_URL=https://links.example.org \
DATA_DIR=/var/lib/shl \
node src/server.mjs
```

or as a container, built from the same directory:

```bash
docker build -t refhir-link-host link-host
docker run -d -p 8090:8090 -v shl-data:/data \
  -e ADMIN_TOKEN=<a secret of 16 characters or more> \
  -e PUBLIC_URL=https://links.example.org \
  refhir-link-host
```

`PUBLIC_URL` is the address the host is reached at, and is what the links it makes point to; put
the host behind a reverse proxy that serves it over HTTPS. Its README lists the settings, every
request it answers, and what to know before deploying it.

A host has to follow the manifest protocol of the specification, and any that does works the same
way for the receiver. How a sender puts a file on a host and gets a link address is up to the
host, because the specification leaves it out, so the steps below use this one's requests.

## The sender

This script does the part between ReFHIR and the host: it fetches the summary, encrypts it, and
makes the link.

```js
// Make a SMART Health Link for one patient's summary.
//
//   FHIR_BASE=https://refhir.example.org/fhir ACCESS_TOKEN=... PATIENT_ID=p1 \
//   MANIFEST_URL=https://links.example.org/shl/abc123/manifest node share-summary.mjs
//
// Set LINK_FLAGS to P for a link that asks for a passcode, or L for one that is meant to last.
//
// It asks ReFHIR for the summary, encrypts it with a key made here, writes the encrypted file to
// summary.jwe for you to put on the link host, and prints the link. The key is in the link and
// nowhere else: the host never has it.
import { writeFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';

const base64url = (bytes) => Buffer.from(bytes).toString('base64url');

// A JWE in compact form, "alg": "dir" and "enc": "A256GCM", as the SMART Health Links
// specification asks of a file. `key` is 32 bytes.
export async function encryptFile(text, key, contentType = 'application/fhir+json') {
  const header = base64url(JSON.stringify({ alg: 'dir', enc: 'A256GCM', cty: contentType }));
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const secret = await crypto.subtle.importKey('raw', key, 'AES-GCM', false, ['encrypt']);
  const sealed = new Uint8Array(
    await crypto.subtle.encrypt(
      // The protected header is authenticated along with the content.
      { name: 'AES-GCM', iv, additionalData: new TextEncoder().encode(header), tagLength: 128 },
      secret,
      new TextEncoder().encode(text),
    ),
  );
  const ciphertext = sealed.slice(0, -16);
  const tag = sealed.slice(-16);
  // header . (empty encrypted key) . iv . ciphertext . tag
  return `${header}..${base64url(iv)}.${base64url(ciphertext)}.${base64url(tag)}`;
}

// The link: `shlink:/` and the base64url of a JSON payload that names the manifest and holds the key.
export function makeLink({ manifestUrl, key, label, expiresAt, flag = '' }) {
  const payload = { url: manifestUrl, key: base64url(key), label, exp: expiresAt };
  if (flag) payload.flag = flag;
  return `shlink:/${base64url(JSON.stringify(payload))}`;
}

async function main() {
  const { FHIR_BASE, ACCESS_TOKEN, PATIENT_ID, MANIFEST_URL } = process.env;
  if (!FHIR_BASE || !ACCESS_TOKEN || !PATIENT_ID || !MANIFEST_URL) {
    console.error('Set FHIR_BASE, ACCESS_TOKEN, PATIENT_ID and MANIFEST_URL.');
    process.exit(1);
  }
  const response = await fetch(`${FHIR_BASE}/Patient/${PATIENT_ID}/$summary`, {
    headers: { authorization: `Bearer ${ACCESS_TOKEN}`, accept: 'application/fhir+json' },
  });
  if (!response.ok) throw new Error(`$summary answered ${response.status}`);
  const key = crypto.getRandomValues(new Uint8Array(32));
  await writeFile('summary.jwe', await encryptFile(await response.text(), key));
  const week = 7 * 24 * 60 * 60;
  console.log(makeLink({
    manifestUrl: MANIFEST_URL,
    key,
    label: 'Patient summary',
    expiresAt: Math.floor(Date.now() / 1000) + week,
    flag: process.env.LINK_FLAGS ?? '',
  }));
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
```

It writes `summary.jwe`, which you give to the host, and prints the link. `MANIFEST_URL` is the
address the host gave you for the link. Set `LINK_FLAGS` to `P` for a link that asks for a
passcode, `U` for one whose address is the file, or `L` for one that is meant to last. The
encrypted file is a compact JWE (`alg` `dir`, `enc` `A256GCM`), as the specification asks of a
file.

The script is not a SMART app: it takes a token you have. A sender that is one registers as
described in [SMART on FHIR](smart.md), signs the patient in, and asks for the scopes it needs,
`patient/Patient.rs` and a read scope for each kind of resource the summary should hold.

## Put it together

```bash
# 1. The host makes a link, with a passcode.
curl -s -X POST https://links.example.org/shl \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -d '{"passcode": "open sesame"}'
#    {"id": "...", "manifestUrl": "https://links.example.org/shl/...", "manageToken": "...", ...}

# 2. The sender fetches the summary, encrypts it, and prints the link.
FHIR_BASE=https://refhir.example.org/fhir ACCESS_TOKEN=$PATIENT_TOKEN PATIENT_ID=p1 \
MANIFEST_URL=<the manifestUrl> LINK_FLAGS=P node share-summary.mjs

# 3. The encrypted file goes on the link.
curl -s -X POST "https://links.example.org/shl/<id>/file?contentType=application%2Ffhir%2Bjson%3BfhirVersion%3D4.0.1" \
  -H "Authorization: Bearer <manageToken>" -H "Content-Type: application/jose" \
  --data-binary @summary.jwe
```

Show the link from step 2 as a QR code, or send it. Whoever opens it is asked for the passcode,
which you tell them another way. To see what they would be given, ask the manifest yourself:

```bash
curl -s -X POST "<manifestUrl>" -d '{"recipient": "me", "passcode": "open sesame"}'
```

## Keep it safe

- **A link is a key.** Anyone who has it can read what it points at, until it expires or is
  switched off. Give it an expiry, as the script does, and a passcode (flag `P`) when it will pass
  through places you do not control.
- **Keep the host apart** from the FHIR server: another address, another credential, and no
  access to the database. A host that holds only ciphertext is a much smaller thing to lose.
- **Limit what the summary holds.** The summary holds only what the sender's token may read, so
  ask the patient for the scopes that match what they want to share.
- **Switch links off** when they are no longer needed, with `DELETE /shl/{id}`. A link that is gone
  stops working at once; a copy of the data that was already opened is out of everyone's hands.
- **Read the access list** (`GET /shl/{id}/access`) for a link that matters, and rate-limit the
  host's manifest URL, whose passcode attempts are the only thing between a guesser and the data.

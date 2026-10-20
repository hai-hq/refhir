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

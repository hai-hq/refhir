import { webcrypto } from 'node:crypto';

const base64url = (bytes) => Buffer.from(bytes).toString('base64url');

// A file the way a sender makes it: a compact JWE, direct and A256GCM. The host never opens one;
// the tests make them so the host has something of the right shape to keep.
export async function encryptFile(text, key, contentType = 'application/fhir+json') {
  const header = base64url(JSON.stringify({ alg: 'dir', enc: 'A256GCM', cty: contentType }));
  const iv = webcrypto.getRandomValues(new Uint8Array(12));
  const secret = await webcrypto.subtle.importKey('raw', key, 'AES-GCM', false, ['encrypt']);
  const sealed = new Uint8Array(
    await webcrypto.subtle.encrypt(
      { name: 'AES-GCM', iv, additionalData: new TextEncoder().encode(header), tagLength: 128 },
      secret,
      new TextEncoder().encode(text),
    ),
  );
  return `${header}..${base64url(iv)}.${base64url(sealed.slice(0, -16))}.${base64url(sealed.slice(-16))}`;
}

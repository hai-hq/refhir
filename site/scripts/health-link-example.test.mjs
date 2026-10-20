import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { createDecipheriv } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { pathToFileURL } from 'node:url';
import { promisify } from 'node:util';

import { repoRoot } from './lib/repo.mjs';

const run = promisify(execFile);
const scriptPath = path.join(repoRoot, 'docs/examples/share-summary.mjs');
const { encryptFile, makeLink } = await import(pathToFileURL(scriptPath));

// What a host's reader does with a file: split the compact JWE and open it with the key from the link.
function decrypt(jwe, key) {
  const [header, encryptedKey, iv, ciphertext, tag] = jwe.split('.');
  assert.equal(encryptedKey, '', 'direct encryption has no encrypted key');
  const decipher = createDecipheriv('aes-256-gcm', key, Buffer.from(iv, 'base64url'));
  decipher.setAAD(Buffer.from(header));
  decipher.setAuthTag(Buffer.from(tag, 'base64url'));
  const text = Buffer.concat([decipher.update(Buffer.from(ciphertext, 'base64url')), decipher.final()]);
  return { header: JSON.parse(Buffer.from(header, 'base64url')), text: text.toString('utf8') };
}

const parseLink = (link) => {
  assert.ok(link.startsWith('shlink:/'));
  return JSON.parse(Buffer.from(link.slice('shlink:/'.length), 'base64url'));
};

test('a file is a compact JWE that its key opens and no other key does', async () => {
  const key = crypto.getRandomValues(new Uint8Array(32));
  const text = JSON.stringify({ resourceType: 'Bundle', type: 'document' });
  const jwe = await encryptFile(text, key);
  assert.equal(jwe.split('.').length, 5);
  const opened = decrypt(jwe, key);
  assert.deepEqual(opened.header, { alg: 'dir', enc: 'A256GCM', cty: 'application/fhir+json' });
  assert.equal(opened.text, text);
  assert.throws(() => decrypt(jwe, crypto.getRandomValues(new Uint8Array(32))));
  const tampered = jwe.replace(/\.([^.]+)\.([^.]+)$/, (_, ciphertext, tag) => `.${ciphertext}A.${tag}`);
  assert.throws(() => decrypt(tampered, key));
});

test('two files made with one key do not share an iv', async () => {
  const key = crypto.getRandomValues(new Uint8Array(32));
  const [first, second] = await Promise.all([encryptFile('a', key), encryptFile('a', key)]);
  assert.notEqual(first.split('.')[2], second.split('.')[2]);
});

test('a link holds the manifest address and the key, and a flag only when there is one', () => {
  const key = crypto.getRandomValues(new Uint8Array(32));
  const link = makeLink({ manifestUrl: 'https://links.example/shl/1/manifest', key, label: 'Patient summary', expiresAt: 1900000000 });
  const payload = parseLink(link);
  assert.equal(payload.url, 'https://links.example/shl/1/manifest');
  assert.equal(payload.key, Buffer.from(key).toString('base64url'));
  assert.equal(payload.key.length, 43, 'a 256-bit key is 43 base64url characters');
  assert.equal(payload.exp, 1900000000);
  assert.equal(payload.label, 'Patient summary');
  assert.ok(!('flag' in payload));
  assert.equal(parseLink(makeLink({ manifestUrl: 'https://x/m', key, label: 'x', expiresAt: 1, flag: 'P' })).flag, 'P');
});

test('the script asks ReFHIR for the summary with the token, and writes a file the link opens', async () => {
  const summary = JSON.stringify({ resourceType: 'Bundle', type: 'document', entry: [] });
  const asked = [];
  const server = http.createServer((request, response) => {
    asked.push({ url: request.url, authorization: request.headers.authorization });
    response.setHeader('content-type', 'application/fhir+json');
    response.end(request.url === '/fhir/Patient/p1/$summary' ? summary : '{}');
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const work = fs.mkdtempSync(path.join(os.tmpdir(), 'share-summary-'));
  try {
    const { stdout } = await run('node', [scriptPath], {
      cwd: work,
      env: {
        ...process.env,
        FHIR_BASE: `http://127.0.0.1:${server.address().port}/fhir`,
        ACCESS_TOKEN: 'patient-token',
        PATIENT_ID: 'p1',
        MANIFEST_URL: 'https://links.example/shl/1/manifest',
      },
    });
    assert.deepEqual(asked, [{ url: '/fhir/Patient/p1/$summary', authorization: 'Bearer patient-token' }]);
    const payload = parseLink(stdout.trim());
    assert.equal(payload.url, 'https://links.example/shl/1/manifest');
    const key = Buffer.from(payload.key, 'base64url');
    const opened = decrypt(fs.readFileSync(path.join(work, 'summary.jwe'), 'utf8'), key);
    assert.equal(opened.text, summary);
    assert.ok(payload.exp > Date.now() / 1000, 'the link has not expired yet');
  } finally {
    server.close();
    fs.rmSync(work, { recursive: true, force: true });
  }
});

test('the script says what it needs, and stops, when it is not given it', async () => {
  await assert.rejects(
    run('node', [scriptPath], { env: { PATH: process.env.PATH } }),
    (error) => error.code === 1 && /Set FHIR_BASE, ACCESS_TOKEN, PATIENT_ID and MANIFEST_URL/.test(error.stderr),
  );
});

test('the guide shows the script that is tested here, word for word', () => {
  const guide = fs.readFileSync(path.join(repoRoot, 'docs/health-links.md'), 'utf8');
  const script = fs.readFileSync(scriptPath, 'utf8').trimEnd();
  assert.ok(guide.includes(script), 'docs/health-links.md and docs/examples/share-summary.mjs have drifted apart');
});

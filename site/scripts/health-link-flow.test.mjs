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
const sender = path.join(repoRoot, 'docs/examples/share-summary.mjs');
const { createHost } = await import(pathToFileURL(path.join(repoRoot, 'link-host/src/host.mjs')));

const ADMIN = 'an-admin-secret-of-length';
const SUMMARY = JSON.stringify({ resourceType: 'Bundle', type: 'document', entry: [{ note: 'private-health-detail' }] });

// A host on a port of its own, with a clock the test can move.
async function startHost(options = {}) {
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'link-host-'));
  let clock = Date.now();
  const server = createHost({ adminToken: ADMIN, dataDir, now: () => clock, ...options });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return {
    base: `http://127.0.0.1:${server.address().port}`,
    dataDir,
    advance: (seconds) => {
      clock += seconds * 1000;
    },
    stop: async () => {
      await new Promise((resolve) => server.close(resolve));
      fs.rmSync(dataDir, { recursive: true, force: true });
    },
  };
}

async function call(url, { method = 'GET', token, body, headers = {} } = {}) {
  const response = await fetch(url, {
    method,
    headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), ...headers },
    body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body),
  });
  const text = await response.text();
  let json;
  try {
    json = JSON.parse(text);
  } catch {
    json = undefined;
  }
  return { status: response.status, text, json, headers: response.headers };
}

const makeLink = (host, options = {}) => call(`${host.base}/shl`, { method: 'POST', token: ADMIN, body: options });
const manifest = (url, body) => call(url, { method: 'POST', body });

async function linkWithFile(host, options = {}, text = SUMMARY) {
  const key = crypto.getRandomValues(new Uint8Array(32));
  const made = (await makeLink(host, options)).json;
  const put = await call(`${host.base}/shl/${made.id}/file?contentType=${encodeURIComponent('application/fhir+json;fhirVersion=4.0.1')}`, {
    method: 'POST',
    token: made.manageToken,
    body: await encryptFile(text, key),
    headers: { 'content-type': 'application/jose' },
  });
  assert.equal(put.status, 201, put.text);
  return { ...made, key };
}

function open(jwe, key) {
  const [header, , iv, ciphertext, tag] = jwe.split('.');
  const decipher = createDecipheriv('aes-256-gcm', key, Buffer.from(iv, 'base64url'));
  decipher.setAAD(Buffer.from(header));
  decipher.setAuthTag(Buffer.from(tag, 'base64url'));
  return Buffer.concat([decipher.update(Buffer.from(ciphertext, 'base64url')), decipher.final()]).toString('utf8');
}

test('a summary goes from ReFHIR to a receiver through the host, which never holds anything readable', async () => {
  const host = await startHost();
  const fhir = http.createServer((request, response) => {
    assert.equal(request.headers.authorization, 'Bearer patient-token');
    response.setHeader('content-type', 'application/fhir+json');
    response.end(SUMMARY);
  });
  await new Promise((resolve) => fhir.listen(0, '127.0.0.1', resolve));
  const work = fs.mkdtempSync(path.join(os.tmpdir(), 'sender-'));
  try {
    // The host makes the link, and the sender makes the key and the encrypted file.
    const made = (await makeLink(host, { passcode: 'correct horse' })).json;
    assert.equal(made.passcodeRequired, true);
    const { stdout } = await run('node', [sender], {
      cwd: work,
      env: {
        ...process.env,
        FHIR_BASE: `http://127.0.0.1:${fhir.address().port}/fhir`,
        ACCESS_TOKEN: 'patient-token',
        PATIENT_ID: 'p1',
        MANIFEST_URL: made.manifestUrl,
        LINK_FLAGS: 'P',
      },
    });
    const link = stdout.trim();
    const payload = JSON.parse(Buffer.from(link.slice('shlink:/'.length), 'base64url'));
    assert.equal(payload.flag, 'P');
    assert.equal(payload.url, made.manifestUrl);

    const upload = await call(`${host.base}/shl/${made.id}/file`, {
      method: 'POST',
      token: made.manageToken,
      body: fs.readFileSync(path.join(work, 'summary.jwe'), 'utf8'),
      headers: { 'content-type': 'application/jose' },
    });
    assert.equal(upload.status, 201, upload.text);

    // The receiver opens the link: the manifest, then the file, then the key from the link.
    const asked = await manifest(payload.url, { recipient: 'Dr Okafor', passcode: 'correct horse' });
    assert.equal(asked.status, 200, asked.text);
    assert.equal(asked.json.files.length, 1);
    assert.equal(asked.json.files[0].contentType, 'application/fhir+json');
    const fetched = await call(asked.json.files[0].location);
    assert.equal(fetched.status, 200);
    assert.equal(fetched.headers.get('content-type'), 'application/jose');
    assert.equal(open(fetched.text, Buffer.from(payload.key, 'base64url')), SUMMARY);

    // Nothing the host holds, sent or stored, can be read without the key.
    const stored = fs.readdirSync(path.join(host.dataDir, made.id)).map((name) => fs.readFileSync(path.join(host.dataDir, made.id, name), 'utf8'));
    for (const text of [...stored, fetched.text, asked.text, upload.text]) {
      assert.ok(!text.includes('private-health-detail') && !text.includes('Bundle') && !text.includes(payload.key));
    }
  } finally {
    fhir.close();
    fs.rmSync(work, { recursive: true, force: true });
    await host.stop();
  }
});

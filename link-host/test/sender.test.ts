import { execFile } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { promisify } from 'node:util';

import { expect, it } from 'vitest';

import { SUMMARY, call, makeLink, manifest, open, using } from './support.js';

const run = promisify(execFile);
// The sender is the example in the ReFHIR repository's docs, which a deployment would copy.
const sender = path.resolve(import.meta.dirname, '../../docs/examples/share-summary.mjs');

it('takes a summary from ReFHIR to a receiver through the host, which never holds anything readable', () =>
  using(async (host) => {
    const fhir = http.createServer((request, response) => {
      expect(request.headers.authorization).toBe('Bearer patient-token');
      response.setHeader('content-type', 'application/fhir+json');
      response.end(SUMMARY);
    });
    await new Promise<void>((resolve) => fhir.listen(0, '127.0.0.1', resolve));
    const work = fs.mkdtempSync(path.join(os.tmpdir(), 'sender-'));
    try {
      // The host makes the link, and the sender makes the key and the encrypted file.
      const made = (await makeLink(host, { passcode: 'correct horse' })).json;
      expect(made.passcodeRequired).toBe(true);
      const { stdout } = await run('node', [sender], {
        cwd: work,
        env: {
          ...process.env,
          FHIR_BASE: `http://127.0.0.1:${(fhir.address() as AddressInfo).port}/fhir`,
          ACCESS_TOKEN: 'patient-token',
          PATIENT_ID: 'p1',
          MANIFEST_URL: made.manifestUrl,
          LINK_FLAGS: 'P',
        },
      });
      const link = stdout.trim();
      const payload = JSON.parse(Buffer.from(link.slice('shlink:/'.length), 'base64url').toString('utf8'));
      expect(payload.flag).toBe('P');
      expect(payload.url).toBe(made.manifestUrl);

      const upload = await call(`${host.base}/shl/${made.id}/file`, {
        method: 'POST',
        token: made.manageToken,
        body: fs.readFileSync(path.join(work, 'summary.jwe'), 'utf8'),
        headers: { 'content-type': 'application/jose' },
      });
      expect(upload.status, upload.text).toBe(201);

      // The receiver opens the link: the manifest, then the file, then the key from the link.
      const asked = await manifest(payload.url, { recipient: 'Dr Okafor', passcode: 'correct horse' });
      expect(asked.status, asked.text).toBe(200);
      expect(asked.json.files).toHaveLength(1);
      expect(asked.json.files[0].contentType).toBe('application/fhir+json');
      const fetched = await call(asked.json.files[0].location);
      expect(fetched.status).toBe(200);
      expect(fetched.headers.get('content-type')).toBe('application/jose');
      expect(open(fetched.text, Buffer.from(payload.key, 'base64url'))).toBe(SUMMARY);

      // Nothing the host holds, sent or stored, can be read without the key.
      const stored = [(await host.host.store.getFile(made.id, 'f0')) ?? '', JSON.stringify(await host.host.store.get(made.id)), JSON.stringify(await host.host.store.listAccess(made.id))];
      expect(stored[0]).not.toBe('');
      for (const text of [...stored, fetched.text, asked.text, upload.text]) {
        expect(text).not.toContain('private-health-detail');
        expect(text).not.toContain('Bundle');
        expect(text).not.toContain(payload.key);
      }
    } finally {
      fhir.close();
      fs.rmSync(work, { recursive: true, force: true });
    }
  }));

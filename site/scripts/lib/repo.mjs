// What the repository contains, as the website is allowed to see it: files git tracks, plus new
// files that are not ignored. Ignored files (for example the private docs/architecture.md) are
// invisible here, so they can neither be published nor linked to.
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const siteRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
export const repoRoot = path.resolve(siteRoot, '..');

function git(args) {
  return execFileSync('git', args, {
    cwd: repoRoot,
    encoding: 'utf8',
    maxBuffer: 256 * 1024 * 1024,
  })
    .split('\n')
    .filter(Boolean);
}

export function listRepoFiles() {
  const files = new Set([...git(['ls-files']), ...git(['ls-files', '--others', '--exclude-standard'])]);
  const dirs = new Set(['.']);
  for (const file of files) {
    const parts = file.split('/');
    for (let i = 1; i < parts.length; i += 1) dirs.add(parts.slice(0, i).join('/'));
  }
  return { files, dirs };
}

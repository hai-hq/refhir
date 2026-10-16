// Reads the repository files the landing page quotes, at build time. Snippets are extracted from
// the real READMEs, so the page can never show code the project no longer ships, and a README that
// changes shape fails the build instead of silently dropping a snippet.
import fs from 'node:fs';
import path from 'node:path';

import { workspaceVersionFrom } from '../../scripts/lib/cargo.mjs';

// `astro build` and `astro dev` run from site/, so the repository root is one level up.
const repoRoot = path.resolve(process.cwd(), '..');

export function readRepoFile(rel: string): string {
  return fs.readFileSync(path.join(repoRoot, rel), 'utf8');
}

/** The release version, from [workspace.package] in Cargo.toml. */
export function workspaceVersion(): string {
  return workspaceVersionFrom(readRepoFile('Cargo.toml'));
}

/**
 * The text of a fenced code block: the `index`th block whose language is `lang`, inside the
 * section that starts at the `## heading` line (or anywhere in the file when `heading` is null).
 */
export function fence(rel: string, heading: string | null, lang: string, index = 0): string {
  const lines = readRepoFile(rel).replace(/\r\n/g, '\n').split('\n');
  let start = 0;
  let end = lines.length;
  if (heading !== null) {
    start = lines.findIndex((line) => line === `## ${heading}`);
    if (start === -1) throw new Error(`${rel}: no "## ${heading}" section`);
    // The section ends at the next heading that is not inside a code fence: a `# comment` line in a
    // shell block is not a heading.
    let fenced = false;
    for (let i = start + 1; i < lines.length; i += 1) {
      if (/^```/.test(lines[i])) fenced = !fenced;
      else if (!fenced && /^#{1,2} /.test(lines[i])) {
        end = i;
        break;
      }
    }
  }

  const found: string[] = [];
  for (let i = start; i < end; i += 1) {
    const open = lines[i].match(/^```(\w*)\s*$/);
    if (!open) continue;
    const body: string[] = [];
    i += 1;
    while (i < end && !/^```\s*$/.test(lines[i])) {
      body.push(lines[i]);
      i += 1;
    }
    if (open[1] === lang) found.push(body.join('\n'));
  }
  if (found[index] === undefined) {
    throw new Error(`${rel}: no ${lang} block #${index} under "${heading ?? 'the file'}"`);
  }
  return found[index];
}

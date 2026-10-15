import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';

import { sidebar, synced } from '../docs-manifest.mjs';
import { listRepoFiles, siteRoot } from './lib/repo.mjs';

const { files } = listRepoFiles();
const authored = (slug) =>
  ['.md', '.mdx'].some((ext) => fs.existsSync(path.join(siteRoot, 'src/content/docs', `${slug}${ext}`)));
const sidebarSlugs = sidebar.flatMap((group) => group.items);

test('every synced source is a file the repository tracks', () => {
  for (const entry of synced) assert.ok(files.has(entry.source), `${entry.source} is not tracked`);
});

test('private working notes can never be listed', () => {
  for (const entry of synced) assert.doesNotMatch(entry.source, /architecture/i, entry.source);
});

test('slugs and sources are unique and live under docs/', () => {
  assert.equal(new Set(synced.map((e) => e.slug)).size, synced.length, 'duplicate slug');
  assert.equal(new Set(synced.map((e) => e.source)).size, synced.length, 'duplicate source');
  for (const { slug } of synced) assert.match(slug, /^docs\/[a-z0-9][a-z0-9/-]*$/, slug);
});

test('every synced page is in the sidebar, so none is an orphan', () => {
  for (const { slug } of synced) assert.ok(sidebarSlugs.includes(slug), `${slug} is not in the sidebar`);
});

test('every sidebar slug is a synced page or a page written in site/', () => {
  const syncedSlugs = new Set(synced.map((e) => e.slug));
  for (const slug of sidebarSlugs) {
    assert.ok(syncedSlugs.has(slug) || authored(slug), `${slug} has no page`);
  }
});

test('no slug appears in the sidebar twice', () => {
  assert.equal(new Set(sidebarSlugs).size, sidebarSlugs.length);
});

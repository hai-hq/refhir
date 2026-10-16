// Copies the allowlisted canonical docs into src/content/docs/synced/ as Starlight pages, and the
// brand assets from docs/brand/ into public/brand/ and src/assets/brand/. Everything it writes is
// gitignored: the Markdown under docs/ and the files in docs/brand/ stay the single source.
//
//   node scripts/sync-content.mjs
import fs from 'node:fs';
import path from 'node:path';

import { synced } from '../docs-manifest.mjs';
import { listRepoFiles, repoRoot, siteRoot } from './lib/repo.mjs';
import { cliPage, configFilePage, EXAMPLE_TOML, MAIN_RS, MIGRATE_RS } from './lib/reference.mjs';
import { findH1, GITHUB, githubSlug, syncedFileName, transformDocument } from './lib/transform.mjs';

const outDir = path.join(siteRoot, 'src/content/docs/synced');
const brandSource = 'docs/brand';
const brandTargets = [path.join(siteRoot, 'public/brand'), path.join(siteRoot, 'src/assets/brand')];

const { files, dirs } = listRepoFiles();
const errors = [];

const slugs = new Set();
const manifest = new Map();
for (const entry of synced) {
  if (!/^docs\/[a-z0-9][a-z0-9/-]*$/.test(entry.slug)) {
    errors.push(`${entry.source}: slug "${entry.slug}" must be lowercase and start with docs/`);
  }
  if (slugs.has(entry.slug)) errors.push(`${entry.source}: duplicate slug "${entry.slug}"`);
  if (manifest.has(entry.source)) errors.push(`${entry.source}: listed twice`);
  if (!files.has(entry.source)) {
    errors.push(`${entry.source}: not a tracked file (private or ignored files are never published)`);
  }
  slugs.add(entry.slug);
  // The H1 becomes the page title, so a link to its anchor must go to the top of the page. Each
  // page needs to know every other page's H1 anchor, hence this first pass over all of them.
  let h1Slug = null;
  if (files.has(entry.source)) {
    const h1 = findH1(fs.readFileSync(path.join(repoRoot, entry.source), 'utf8').replace(/\r\n/g, '\n').split('\n'));
    if (h1) h1Slug = githubSlug(h1.text);
  }
  manifest.set(entry.source, { ...entry, h1Slug });
}

const context = {
  manifest,
  exists: (p) => files.has(p),
  isDir: (p) => dirs.has(p),
};

fs.rmSync(outDir, { recursive: true, force: true });
fs.mkdirSync(outDir, { recursive: true });

for (const entry of synced) {
  if (!files.has(entry.source)) continue;
  try {
    const text = fs.readFileSync(path.join(repoRoot, entry.source), 'utf8');
    const page = transformDocument({ source: entry.source, text, entry, ...context });
    fs.writeFileSync(path.join(outDir, syncedFileName(entry.slug)), page);
  } catch (error) {
    errors.push(error.message);
  }
}

// Reference pages built from the source: the command-line help and the example configuration.
const generatedPages = [];
try {
  const read = (file) => fs.readFileSync(path.join(repoRoot, file), 'utf8');
  generatedPages.push(cliPage({ mainRs: read(MAIN_RS), migrateRs: read(MIGRATE_RS) }));
  generatedPages.push(configFilePage({ exampleToml: read(EXAMPLE_TOML) }));
} catch (error) {
  errors.push(`reference pages: ${error.message}`);
}
for (const page of generatedPages) {
  if (!files.has(page.source)) errors.push(`${page.source}: not a tracked file`);
  const front = [
    '---',
    `title: ${JSON.stringify(page.title)}`,
    `description: ${JSON.stringify(page.description)}`,
    `slug: ${JSON.stringify(page.slug)}`,
    `editUrl: ${JSON.stringify(`${GITHUB}/edit/main/${page.source}`)}`,
    '---',
    '',
  ].join('\n');
  const note = `<!-- Generated from ${page.source} by site/scripts/sync-content.mjs. Edit that file, not this one. -->`;
  fs.writeFileSync(path.join(outDir, syncedFileName(page.slug)), `${front}${note}\n\n${page.body}`);
}

// The README in docs/brand is documentation, not an asset; it can be synced as a page instead.
const brandFiles = [...files].filter(
  (f) => f.startsWith(`${brandSource}/`) && !f.slice(brandSource.length + 1).includes('/') && !f.endsWith('.md'),
);
for (const target of brandTargets) {
  fs.rmSync(target, { recursive: true, force: true });
  fs.mkdirSync(target, { recursive: true });
  for (const file of brandFiles) fs.copyFileSync(path.join(repoRoot, file), path.join(target, path.basename(file)));
}

if (errors.length > 0) {
  console.error(`sync-content: ${errors.length} problem(s)\n`);
  for (const message of errors) console.error(`  - ${message}`);
  process.exit(1);
}
console.log(
  `sync-content: ${synced.length} synced page(s), ${generatedPages.length} generated, ${brandFiles.length} brand file(s)`,
);

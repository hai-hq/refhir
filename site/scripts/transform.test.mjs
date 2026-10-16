import assert from 'node:assert/strict';
import test from 'node:test';

import { findH1, githubSlug, rewriteDest, syncedFileName, transformDocument } from './lib/transform.mjs';

const FILES = new Set([
  'docs/a.md',
  'docs/b.md',
  'deploy/production.md',
  'crates/server/src/sync.rs',
  'refhir.toml.example',
  'docs/brand/refhir-logo.svg',
]);
const DIRS = new Set(['.', 'docs', 'docs/brand', 'deploy', 'crates', 'crates/server', 'crates/server/src']);

const manifest = new Map([
  ['docs/a.md', { source: 'docs/a.md', slug: 'docs/a' }],
  ['docs/b.md', { source: 'docs/b.md', slug: 'docs/guides/b' }],
]);
const ctx = (source = 'docs/a.md') => ({
  source,
  manifest,
  exists: (p) => FILES.has(p),
  isDir: (p) => DIRS.has(p),
});
const transform = (text, entry = { source: 'docs/a.md', slug: 'docs/a' }) =>
  transformDocument({ source: entry.source, text, entry, manifest, exists: (p) => FILES.has(p), isDir: (p) => DIRS.has(p) });

test('a page in the manifest becomes its site URL and keeps the anchor', () => {
  assert.equal(rewriteDest('b.md#part', ctx()), '/docs/guides/b/#part');
  assert.equal(rewriteDest('./b.md', ctx()), '/docs/guides/b/');
});

test('links resolve relative to the file they are written in', () => {
  assert.equal(rewriteDest('../docs/b.md', ctx('deploy/production.md')), '/docs/guides/b/');
  assert.equal(rewriteDest('../crates/server/src/sync.rs', ctx()), 'https://github.com/hai-hq/refhir/blob/main/crates/server/src/sync.rs');
});

test('files that are not pages go to GitHub, directories to /tree/', () => {
  assert.equal(rewriteDest('../refhir.toml.example', ctx()), 'https://github.com/hai-hq/refhir/blob/main/refhir.toml.example');
  assert.equal(rewriteDest('../crates/server', ctx()), 'https://github.com/hai-hq/refhir/tree/main/crates/server');
});

test('brand images are served from /brand', () => {
  assert.equal(rewriteDest('brand/refhir-logo.svg', ctx()), '/brand/refhir-logo.svg');
});

test('external links, site-absolute links and same-page anchors are untouched', () => {
  assert.equal(rewriteDest('https://example.com/x.md', ctx()), 'https://example.com/x.md');
  assert.equal(rewriteDest('mailto:a@b.c', ctx()), 'mailto:a@b.c');
  assert.equal(rewriteDest('#local', ctx()), '#local');
  assert.equal(rewriteDest('/docs/x/', ctx()), '/docs/x/');
});

test('a missing target throws, with the file and line in transformDocument', () => {
  assert.throws(() => rewriteDest('missing.md', ctx()), /does not exist/);
  assert.throws(() => transform('# T\n\nok\n\nsee [x](missing.md)\n'), /docs\/a\.md:5: link target does not exist/);
});

test('a link that leaves the repository throws', () => {
  assert.throws(() => rewriteDest('../../etc/passwd', ctx()), /leaves the repository/);
});

test('the H1 becomes the title and is removed, and front matter is added', () => {
  const out = transform('# My `Page`\n\nBody text.\n', { source: 'docs/a.md', slug: 'docs/a', description: 'About a' });
  assert.match(out, /^---\ntitle: "My Page"\ndescription: "About a"\nslug: "docs\/a"\neditUrl: "https:\/\/github\.com\/hai-hq\/refhir\/edit\/main\/docs\/a\.md"\n---\n/);
  assert.ok(!out.includes('# My'));
  assert.match(out, /Body text\./);
  assert.match(out, /Generated from docs\/a\.md/);
});

test('a sidebar label is applied', () => {
  const out = transform('# Long title\n\nBody\n', {
    source: 'docs/a.md',
    slug: 'docs/a',
    label: 'Short',
  });
  assert.match(out, /sidebar:\n {2}label: "Short"/);
});

test('a document with no H1 needs a title in the manifest', () => {
  assert.throws(() => transform('Just text.\n'), /no H1 and no title/);
  assert.match(transform('Just text.\n', { source: 'docs/a.md', slug: 'docs/a', title: 'Given' }), /title: "Given"/);
});

test('a # line inside a code fence is not the H1', () => {
  const out = transform('```bash\n# comment\n```\n\n# Real title\n\ntext\n');
  assert.match(out, /title: "Real title"/);
  assert.match(out, /```bash\n# comment\n```/);
});

test('links inside code are left as written, and links around code are rewritten', () => {
  const out = transform('# T\n\nUse `[x](missing.md)` literally, see [`b`](b.md) and ```[y](missing.md)```.\n');
  assert.match(out, /`\[x\]\(missing\.md\)`/);
  assert.match(out, /\[`b`\]\(\/docs\/guides\/b\/\)/);
});

test('links inside code fences are not rewritten and do not fail', () => {
  const out = transform('# T\n\n```md\n[x](missing.md)\n```\n');
  assert.match(out, /\[x\]\(missing\.md\)/);
});

test('reference definitions and HTML attributes are rewritten', () => {
  const out = transform('# T\n\n[ref]: b.md#top\n\n<img src="brand/refhir-logo.svg" alt="x">\n');
  assert.match(out, /\[ref\]: \/docs\/guides\/b\/#top/);
  assert.match(out, /src="\/brand\/refhir-logo\.svg"/);
});

test('link titles are kept', () => {
  assert.match(transform('# T\n\n[x](b.md "More")\n'), /\[x\]\(\/docs\/guides\/b\/ "More"\)/);
});

test('githubSlug matches the anchors GitHub makes', () => {
  assert.equal(githubSlug('Rate limits, quotas, metering and audit'), 'rate-limits-quotas-metering-and-audit');
  assert.equal(githubSlug('Several processes on one database'), 'several-processes-on-one-database');
  assert.equal(githubSlug('The /sync protocol'), 'the-sync-protocol');
  // GitHub keeps accented letters in an anchor.
  assert.equal(githubSlug('Café au lait'), 'café-au-lait');
  assert.equal(githubSlug('`refhir` and [links](x.md)'), 'refhir-and-links');
});

test('findH1 skips fenced code and returns null when there is none', () => {
  assert.deepEqual(findH1(['```', '# not', '```', '# Yes']), { index: 3, text: 'Yes' });
  assert.equal(findH1(['text', '## two']), null);
});

test('a link to another page\'s H1 anchor goes to the top of that page', () => {
  const withH1 = new Map([['docs/b.md', { source: 'docs/b.md', slug: 'docs/guides/b', h1Slug: 'the-b-page' }]]);
  const c = { source: 'docs/a.md', manifest: withH1, exists: (p) => FILES.has(p), isDir: (p) => DIRS.has(p) };
  assert.equal(rewriteDest('b.md#the-b-page', c), '/docs/guides/b/');
  assert.equal(rewriteDest('b.md#another', c), '/docs/guides/b/#another');
});

test('a link to the page\'s own H1 anchor goes to #_top', () => {
  const out = transform('# Own title\n\nBack to [the top](#own-title) or [a section](#other).\n');
  assert.match(out, /\[the top\]\(#_top\)/);
  assert.match(out, /\[a section\]\(#other\)/);
});

test('synced file names flatten the slug', () => {
  assert.equal(syncedFileName('docs/guides/b'), 'docs--guides--b.md');
});

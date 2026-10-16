// Checks the built landing page (dist/index.html) against the rules the site promises. It runs at
// the end of `npm run build`, so a change that breaks a promise fails CI instead of shipping.
//
//   - Every internal link and asset resolves to a file in dist/.
//   - Nothing is loaded from another origin (links to other sites are fine; loading from them is not).
//   - The page has one h1, and every image has alt text.
//   - The page makes none of the claims the project cannot back up (see CLAIMS.md).
//   - The page's JavaScript stays small.
//   - Every built page has what a search engine needs: title, description, canonical, one h1, share tags,
//     and no duplicate titles or descriptions; robots.txt and the sitemap are right.
//   - The page is structurally sound: heading order, unique ids, every reference resolves, every control
//     and link has a name, landmarks, a script-free fallback for the interactive sections, and share
//     metadata that points at files that exist.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { workspaceVersionFrom } from './lib/cargo.mjs';

const dist = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', 'dist');
const JS_BUDGET_BYTES = 15 * 1024;

const FORBIDDEN = [
  /HIPAA/i,
  /enterprise-grade/i,
  /seamless/i,
  /battle-tested/i,
  /production-ready/i,
  /blazing/i,
  /world-class/i,
  /revolutionary/i,
  /\bunlock/i,
  /ops\/s(ec)?\b/i,
  /requests? per second/i,
  // The one allowed use is HAIHQ's own description: "models, datasets, benchmarks and tooling".
  /\bbenchmark(?!s and tooling)/i,
  // "certified" appears only in honest negatives: "not a certified product", "not certified".
  /(?<!not a |not )certified/i,
];

const problems = [];
const fail = (message) => problems.push(message);

function readPage(name) {
  const file = path.join(dist, name);
  if (!fs.existsSync(file)) {
    fail(`${name} was not built`);
    return null;
  }
  return fs.readFileSync(file, 'utf8');
}

function exists(urlPath) {
  const clean = decodeURI(urlPath.split('#')[0].split('?')[0]);
  const target = path.join(dist, clean);
  if (fs.existsSync(target) && fs.statSync(target).isFile()) return true;
  return fs.existsSync(path.join(target, 'index.html'));
}

function attributes(html, tag, attr) {
  const found = [];
  for (const match of html.matchAll(new RegExp(`<${tag}\\b[^>]*>`, 'gi'))) {
    const value = match[0].match(new RegExp(`\\s${attr}="([^"]*)"`, 'i'));
    if (value) found.push({ tag: match[0], value: value[1] });
  }
  return found;
}

// Whether the page a link points at has the element its #fragment names.
function hasAnchor(url) {
  const hash = url.split('#')[1];
  if (!hash) return true;
  let file = path.join(dist, decodeURI(url.split('#')[0].split('?')[0]));
  if (!(fs.existsSync(file) && fs.statSync(file).isFile())) file = path.join(file, 'index.html');
  if (!file.endsWith('.html')) return true;
  return fs.readFileSync(file, 'utf8').includes(` id="${decodeURIComponent(hash)}"`);
}

// The size of a script file and of every chunk it imports, each counted once.
function scriptBytes(file, counted) {
  if (counted.has(file) || !fs.existsSync(file)) return 0;
  counted.add(file);
  const code = fs.readFileSync(file, 'utf8');
  let bytes = Buffer.byteLength(code);
  for (const match of code.matchAll(/(?:\bimport|\bfrom)\s*["'](\.{1,2}\/[^"']+)["']/g)) {
    bytes += scriptBytes(path.resolve(path.dirname(file), match[1]), counted);
  }
  return bytes;
}

const isExternal = (url) => /^(https?:)?\/\//i.test(url);

const landing = readPage('index.html');
if (landing) {
  // Links and assets that stay on the site must exist.
  for (const { value } of [...attributes(landing, 'a', 'href'), ...attributes(landing, 'img', 'src')]) {
    if (isExternal(value) || value.startsWith('mailto:') || value.startsWith('#')) continue;
    if (!exists(value)) fail(`index.html links to ${value}, which is not in dist/`);
    else if (!hasAnchor(value)) fail(`index.html links to ${value}, but that page has no element with that id`);
  }

  // Nothing may be loaded from another origin.
  const loads = [
    ...attributes(landing, 'script', 'src'),
    ...attributes(landing, 'img', 'src'),
    ...attributes(landing, 'iframe', 'src'),
    ...attributes(landing, 'link', 'href').filter(({ tag }) => !/rel="(canonical|alternate|sitemap)"/i.test(tag)),
  ];
  for (const { value } of loads) {
    if (isExternal(value)) fail(`index.html loads ${value} from another origin`);
  }
  if (/url\(\s*["']?https?:/i.test(landing)) fail('index.html has a CSS url() to another origin');

  // Structure.
  for (const match of landing.matchAll(/<img\b[^>]*>/gi)) {
    if (!/\salt=/i.test(match[0])) fail(`an image has no alt attribute: ${match[0].slice(0, 80)}`);
  }

  // Claims. Look at the visible text only, not at attribute values or code.
  const text = landing
    .replace(/<script\b[\s\S]*?<\/script>/gi, ' ')
    .replace(/<style\b[\s\S]*?<\/style>/gi, ' ')
    .replace(/<[^>]+>/g, ' ')
    .replace(/&amp;/g, '&')
    .replace(/\s+/g, ' ');
  for (const pattern of FORBIDDEN) {
    const hit = text.match(pattern);
    if (hit) fail(`index.html contains "${hit[0]}", which the project cannot back up (${pattern})`);
  }

  // Structure and accessibility that a browser would not complain about but a visitor would notice.
  const stripTags = (html) => html.replace(/<[^>]+>/g, ' ').replace(/&[a-z#0-9]+;/gi, ' ').replace(/\s+/g, ' ').trim();

  // Heading order: starts at h1 and never skips a level going down.
  let previous = 0;
  for (const match of landing.matchAll(/<h([1-6])\b/gi)) {
    const level = Number(match[1]);
    if (previous === 0 && level !== 1) fail(`the first heading is an h${level}, expected h1`);
    if (previous !== 0 && level > previous + 1) fail(`a heading jumps from h${previous} to h${level}`);
    previous = level;
  }

  // Unique ids, and every reference to an id resolves.
  const ids = [...landing.matchAll(/\sid="([^"]+)"/g)].map((m) => m[1]);
  const seen = new Set();
  for (const id of ids) {
    if (seen.has(id)) fail(`duplicate id "${id}"`);
    seen.add(id);
  }
  const refs = [
    ...[...landing.matchAll(/\saria-labelledby="([^"]+)"/g)].flatMap((m) => m[1].split(/\s+/)),
    ...[...landing.matchAll(/\sfor="([^"]+)"/g)].map((m) => m[1]),
    ...[...landing.matchAll(/\shref="#([^"]+)"/g)].map((m) => m[1]),
  ];
  for (const ref of refs) if (!seen.has(ref)) fail(`a reference points at #${ref}, which does not exist`);

  // Every button and link has a name.
  for (const match of landing.matchAll(/<(button|a)\b([^>]*)>([\s\S]*?)<\/\1>/gi)) {
    const [, tag, attrs, inner] = match;
    if (tag.toLowerCase() === 'a' && !/\shref=/.test(attrs)) continue;
    const named = /\saria-label="[^"]+"/.test(attrs) || stripTags(inner) !== '' || /<img\b[^>]*\salt="[^"]+"/i.test(inner);
    if (!named) fail(`a ${tag} has no accessible name: ${match[0].slice(0, 90)}`);
  }

  // Every radio has a label, and every details has a summary.
  for (const match of landing.matchAll(/<input\b[^>]*type="radio"[^>]*>/gi)) {
    const id = match[0].match(/\sid="([^"]+)"/)?.[1];
    if (!id || !new RegExp(`<label\\b[^>]*\\sfor="${id}"`).test(landing)) fail(`a radio input has no label: ${match[0].slice(0, 80)}`);
  }
  for (const match of landing.matchAll(/<details\b[\s\S]*?<\/details>/gi)) {
    if (!/<summary\b/i.test(match[0])) fail('a details element has no summary');
  }

  // Landmarks.
  if ([...landing.matchAll(/<main\b/gi)].length !== 1) fail('the page needs exactly one main landmark');
  for (const tag of ['header', 'footer', 'nav']) if (!new RegExp(`<${tag}\\b`, 'i').test(landing)) fail(`the page has no ${tag} landmark`);
  if (!/<a\b[^>]*class="skip"[^>]*href="#main"/.test(landing)) fail('the skip link to #main is missing');

  // The interactive sections say what to do without JavaScript.
  if ([...landing.matchAll(/<noscript\b/gi)].length < 2) fail('each interactive section needs a <noscript> note');

  // Search and sharing metadata.
  // The title, description, canonical, h1 and the main og tags are checked for every page below.
  if (!/<html\b[^>]*\slang="en"/i.test(landing)) fail('the html element has no lang');
  for (const property of ['og:image:alt', 'twitter:card', 'twitter:image:alt']) {
    if (!new RegExp(`(?:property|name)="${property}"`, 'i').test(landing)) fail(`missing ${property} metadata`);
  }
  // The share image: the file the tags name exists, and its real size is the size they declare.
  const content = (name, kind = 'property') => landing.match(new RegExp(`<meta ${kind}="${name}" content="([^"]*)"`, 'i'))?.[1];
  const imageUrl = content('og:image');
  if (!imageUrl) fail('og:image is missing');
  else {
    if (content('twitter:image', 'name') !== imageUrl) fail('twitter:image does not match og:image');
    const file = path.join(dist, new URL(imageUrl).pathname);
    if (!fs.existsSync(file)) fail(`the share image ${imageUrl} is not in dist/`);
    else {
      const bytes = fs.readFileSync(file);
      const [w, h] = [bytes.readUInt32BE(16), bytes.readUInt32BE(20)];
      if (bytes.subarray(1, 4).toString() !== 'PNG') fail('the share image is not a PNG');
      if (String(w) !== content('og:image:width') || String(h) !== content('og:image:height')) {
        fail(`the share image is ${w}x${h}, but the tags declare ${content('og:image:width')}x${content('og:image:height')}`);
      }
      if (w !== 1200 || h !== 630) fail(`the share image is ${w}x${h}, expected the 1200x630 that link previews use`);
      if (bytes.length > 600 * 1024) fail(`the share image is ${bytes.length} bytes, over the 600 KB some platforms allow`);
    }
  }
  if (/noindex/i.test(landing.match(/<meta name="robots"[^>]*>/i)?.[0] ?? '')) fail('the landing page is marked noindex');
  if (!/<meta name="robots" content="[^"]*index[^"]*"/i.test(landing)) fail('the landing page has no robots meta');

  // Structured data: parses, names the right things, and says what the repository says.
  const ld = landing.match(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/i)?.[1];
  if (!ld) fail('the landing page has no JSON-LD');
  else {
    let data = null;
    try {
      data = JSON.parse(ld);
    } catch (error) {
      fail(`the JSON-LD does not parse: ${error.message}`);
    }
    const graph = data?.['@graph'] ?? [];
    const byType = (type) => graph.find((node) => node['@type'] === type);
    for (const type of ['WebSite', 'Organization', 'SoftwareApplication']) if (!byType(type)) fail(`the JSON-LD has no ${type}`);
    const software = byType('SoftwareApplication');
    if (software) {
      const version = workspaceVersionFrom(fs.readFileSync(path.resolve(dist, '..', '..', 'Cargo.toml'), 'utf8'));
      if (software.softwareVersion !== version) fail(`the JSON-LD says version ${software.softwareVersion}, Cargo.toml says ${version}`);
      if (software.license !== 'https://www.apache.org/licenses/LICENSE-2.0') fail('the JSON-LD license is not Apache-2.0');
      if (software.url !== landing.match(/<link rel="canonical" href="([^"]*)"/i)?.[1]) fail('the JSON-LD url is not the canonical url');
      if (software.image !== imageUrl) fail('the JSON-LD image is not the share image');
    }
  }

  // Icons.
  const ico = path.join(dist, 'favicon.ico');
  if (!fs.existsSync(ico) || fs.readFileSync(ico).readUInt32BE(0) !== 0x00000100) fail('favicon.ico is missing or is not an ICO file');
  const touch = path.join(dist, 'apple-touch-icon.png');
  if (!fs.existsSync(touch)) fail('apple-touch-icon.png is missing');
  else {
    const bytes = fs.readFileSync(touch);
    if (bytes.readUInt32BE(16) !== 180 || bytes.readUInt32BE(20) !== 180) fail('apple-touch-icon.png is not 180x180');
  }
  if (!/<link rel="preload" as="font" type="font\/woff2" href="([^"]+)"/i.test(landing)) fail('the main font is not preloaded');

  // JavaScript budget: the scripts the page loads and every chunk they import, plus any inline script.
  let bytes = 0;
  const counted = new Set();
  for (const { value } of attributes(landing, 'script', 'src')) {
    if (!isExternal(value) && exists(value)) bytes += scriptBytes(path.join(dist, value.split('?')[0]), counted);
  }
  for (const match of landing.matchAll(/<script\b(?![^>]*\ssrc=)(?![^>]*type=\"application\/ld\+json\")[^>]*>([\s\S]*?)<\/script>/gi)) {
    bytes += Buffer.byteLength(match[1]);
  }
  if (bytes > JS_BUDGET_BYTES) fail(`the landing page ships ${bytes} bytes of JavaScript, over the ${JS_BUDGET_BYTES} budget`);
  console.log(`check-built: landing page JavaScript is ${bytes} bytes (budget ${JS_BUDGET_BYTES})`);
}

// Every page, not only the landing page, is something a search engine indexes: it needs a title, a
// description, an absolute canonical url, one h1 and its share tags, and none may repeat another's
// title or description. Redirect stubs and the 404 page are not indexed content.
const ORIGIN = 'https://refhir.haihq.org';
const pages = [];
const walk = (dir) => {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name !== 'pagefind' && entry.name !== '_astro') walk(full);
    } else if (entry.name === 'index.html') pages.push(full);
  }
};
walk(dist);
const titles = new Map();
const descriptions = new Map();
for (const file of pages) {
  const html = fs.readFileSync(file, 'utf8');
  const name = path.relative(dist, file) || 'index.html';
  if (/http-equiv="refresh"/i.test(html)) continue;
  const title = html.match(/<title>([^<]*)<\/title>/i)?.[1] ?? '';
  const description = html.match(/<meta name="description" content="([^"]*)"/i)?.[1] ?? '';
  const canonical = html.match(/<link rel="canonical" href="([^"]*)"/i)?.[1] ?? '';
  if (title.length < 5 || title.length > 70) fail(`${name}: the title is ${title.length} characters, expected 5 to 70`);
  if (description.length < 50 || description.length > 170) fail(`${name}: the description is ${description.length} characters, expected 50 to 170`);
  if (!canonical.startsWith(`${ORIGIN}/`) || !canonical.endsWith('/')) fail(`${name}: the canonical url "${canonical}" is not an absolute ${ORIGIN} url ending in a slash`);
  if ([...html.matchAll(/<h1\b/gi)].length !== 1) fail(`${name}: the page needs exactly one h1`);
  for (const property of ['og:title', 'og:description', 'og:image', 'og:url']) {
    if (!new RegExp(`property="${property}"`, 'i').test(html)) fail(`${name}: missing ${property}`);
  }
  if (/<meta name="robots"[^>]*noindex/i.test(html)) fail(`${name}: marked noindex`);
  for (const [map, value, label] of [[titles, title, 'title'], [descriptions, description, 'description']]) {
    if (map.has(value)) fail(`${name} repeats the ${label} of ${map.get(value)}`);
    map.set(value, name);
  }
}

// Crawl files.
const robots = fs.existsSync(path.join(dist, 'robots.txt')) ? fs.readFileSync(path.join(dist, 'robots.txt'), 'utf8') : '';
if (!robots.includes(`Sitemap: ${ORIGIN}/sitemap-index.xml`)) fail('robots.txt does not point at the sitemap');
if (/^\s*Disallow:\s*\/\s*$/im.test(robots)) fail('robots.txt blocks the whole site');
const sitemap = fs.existsSync(path.join(dist, 'sitemap-0.xml')) ? fs.readFileSync(path.join(dist, 'sitemap-0.xml'), 'utf8') : '';
if (!sitemap.includes(`<loc>${ORIGIN}/</loc>`)) fail('the sitemap does not list the landing page');
if (!sitemap.includes(`<loc>${ORIGIN}/docs/introduction/</loc>`)) fail('the sitemap does not list the docs');

const notFound = readPage('404.html');
if (notFound) {
  for (const { value } of attributes(notFound, 'a', 'href')) {
    if (isExternal(value) || value.startsWith('mailto:') || value.startsWith('#')) continue;
    if (!exists(value)) fail(`404.html links to ${value}, which is not in dist/`);
  }
}

for (const file of ['robots.txt', 'sitemap-index.xml', '_headers']) {
  if (!fs.existsSync(path.join(dist, file))) fail(`${file} is missing from dist/`);
}

if (problems.length > 0) {
  console.error(`check-built: ${problems.length} problem(s)\n`);
  for (const message of problems) console.error(`  - ${message}`);
  process.exit(1);
}
console.log('check-built: the landing page keeps its promises');

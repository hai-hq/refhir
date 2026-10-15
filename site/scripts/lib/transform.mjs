// Pure text transforms that turn a canonical repository document (plain GitHub Markdown, relative
// links, no front matter) into a Starlight page. No file system access here: callers pass in
// `exists` and `isDir`, so this is easy to test.
import path from 'node:path';

export const GITHUB = 'https://github.com/hai-hq/refhir';

const FENCE = /^\s*(```|~~~)/;
// A scheme (https:, mailto:), a protocol-relative or site-absolute URL, or a same-page anchor.
const LEAVE_ALONE = /^([a-z][a-z0-9+.-]*:|\/|#)/i;

const INLINE_LINK = /(!?\[[^\]]*\]\()([^)\s]+)((?:\s+"[^"]*")?\))/g;
const REFERENCE_DEFINITION = /^(\s*\[[^\]]+\]:\s*)(\S+)/;
const HTML_ATTRIBUTE = /(\s(?:src|href)=")([^"]+)(")/g;

function splitFragment(dest) {
  const i = dest.indexOf('#');
  return i === -1 ? [dest, ''] : [dest.slice(0, i), dest.slice(i)];
}

/**
 * Rewrite one link destination found in `ctx.source`.
 *
 * - A page in the manifest becomes its site URL, keeping the anchor.
 * - A brand image becomes /brand/<file>.
 * - Any other tracked file or directory becomes a GitHub URL.
 * - A target that does not exist throws, so a broken link fails the build.
 */
export function rewriteDest(dest, ctx) {
  if (LEAVE_ALONE.test(dest)) return dest;
  const [rel, fragment] = splitFragment(dest);
  const target = path.posix.normalize(path.posix.join(path.posix.dirname(ctx.source), decodeURI(rel)));
  if (target === '..' || target.startsWith('../')) {
    throw new Error(`link leaves the repository: ${dest}`);
  }
  const clean = target.replace(/\/+$/, '') || '.';

  const page = ctx.manifest.get(clean);
  if (page) return `/${page.slug}/${fragment}`;
  if (/^docs\/brand\/[^/]+\.(svg|png)$/.test(clean)) return `/brand/${path.posix.basename(clean)}`;
  if (ctx.isDir(clean)) return `${GITHUB}/tree/main/${clean}${fragment}`;
  if (ctx.exists(clean)) return `${GITHUB}/blob/main/${clean}${fragment}`;
  throw new Error(`link target does not exist: ${dest} (resolved to ${clean})`);
}

// Inline code is masked first so that `code` inside link text does not break link matching and
// links written inside code are left exactly as they are.
function rewriteLine(line, ctx) {
  const spans = [];
  const masked = line.replace(/`+[^`]*`+/g, (m) => {
    spans.push(m);
    return `\u0000${spans.length - 1}\u0000`;
  });
  const out = masked
    .replace(INLINE_LINK, (_, open, dest, close) => open + rewriteDest(dest, ctx) + close)
    .replace(REFERENCE_DEFINITION, (_, open, dest) => open + rewriteDest(dest, ctx))
    .replace(HTML_ATTRIBUTE, (_, open, dest, close) => open + rewriteDest(dest, ctx) + close);
  return out.replace(/\u0000(\d+)\u0000/g, (_, i) => spans[Number(i)]);
}

function cleanTitle(text) {
  return text.replace(/[`*_]/g, '').replace(/\s*#+\s*$/, '').trim();
}

/**
 * Build the text of a Starlight page from a canonical document.
 *
 * `entry` is the manifest entry: { source, slug, title?, label?, description?, preface? }.
 * The first H1 becomes the page title and is removed from the body, because Starlight renders
 * the title itself.
 */
export function transformDocument({ source, text, entry, manifest, exists, isDir }) {
  const lines = text.replace(/\r\n/g, '\n').split('\n');

  let fenced = false;
  let h1 = -1;
  let title = entry.title ?? null;
  lines.forEach((line, i) => {
    if (FENCE.test(line)) {
      fenced = !fenced;
      return;
    }
    if (!fenced && h1 === -1 && /^# /.test(line)) {
      h1 = i;
      title ??= cleanTitle(line.slice(2));
    }
  });
  if (!title) throw new Error(`${source}: no H1 and no title in the manifest`);

  const ctx = { source, manifest, exists, isDir };
  const body = [];
  fenced = false;
  let skipBlank = false;
  lines.forEach((line, i) => {
    if (FENCE.test(line)) {
      fenced = !fenced;
      body.push(line);
      return;
    }
    if (fenced) {
      body.push(line);
      return;
    }
    if (i === h1) {
      skipBlank = true;
      return;
    }
    if (skipBlank && line.trim() === '') {
      skipBlank = false;
      return;
    }
    skipBlank = false;
    try {
      body.push(rewriteLine(line, ctx));
    } catch (error) {
      throw new Error(`${source}:${i + 1}: ${error.message}`);
    }
  });

  const front = ['---', `title: ${JSON.stringify(title)}`];
  if (entry.description) front.push(`description: ${JSON.stringify(entry.description)}`);
  front.push(`slug: ${JSON.stringify(entry.slug)}`);
  front.push(`editUrl: ${JSON.stringify(`${GITHUB}/edit/main/${source}`)}`);
  if (entry.label) front.push('sidebar:', `  label: ${JSON.stringify(entry.label)}`);
  front.push('---', '');

  const generated = `<!-- Generated from ${source} by site/scripts/sync-content.mjs. Edit that file, not this one. -->`;
  const parts = [front.join('\n'), generated, ''];
  if (entry.preface) parts.push(entry.preface.trim(), '');
  parts.push(body.join('\n').replace(/^\n+/, ''));
  return parts.join('\n');
}

/** File name for a synced page: the slug with slashes flattened. */
export function syncedFileName(slug) {
  return `${slug.replace(/\//g, '--')}.md`;
}

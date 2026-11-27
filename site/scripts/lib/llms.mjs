// The docs as plain Markdown, for AI coding assistants: /llms.txt (an index, in the llmstxt.org
// format), /llms-full.txt (every page in one file) and a .md copy beside each page
// (/docs/tutorial.md beside /docs/tutorial/). The endpoints in src/pages/ call these functions with
// the docs collection; they are plain functions so scripts/llms.test.mjs can test them.

/** Where a page's Markdown copy is served, as a path: docs/tutorial -> /docs/tutorial.md */
export const markdownPath = (slug) => `/${slug}.md`;

// The Starlight components the .mdx pages use, and what each becomes in plain Markdown. A page
// that uses any other component fails the build, so a new one is handled here rather than leaking
// into the Markdown as a raw tag.
// ArticleList (the articles index) is dropped too: llms.txt lists every article under Articles.
const DROPPED = ['Tabs', 'Steps', 'ArticleList'];
const ASIDE_LABEL = { note: 'Note', tip: 'Tip', caution: 'Caution', danger: 'Danger' };

const attr = (attrs, name) => attrs.match(new RegExp(`\\b${name}="([^"]*)"`))?.[1];

/** Removes the common leading whitespace of a block of lines, ignoring blank lines. */
function dedent(lines) {
  const indents = lines.filter((l) => l.trim() !== '').map((l) => l.match(/^ */)[0].length);
  const cut = indents.length ? Math.min(...indents) : 0;
  return lines.map((l) => l.slice(cut));
}

const trimBlank = (lines) => {
  let start = 0;
  let end = lines.length;
  while (start < end && lines[start].trim() === '') start++;
  while (end > start && lines[end - 1].trim() === '') end--;
  return lines.slice(start, end);
};

/**
 * Turns the body of a docs page into plain Markdown. For .mdx pages it drops imports and replaces
 * the Starlight components with what they show: an <Aside> becomes a quote with its title, a
 * <TabItem> a heading with its label (one level below the heading it sits under), and <Tabs> and
 * <Steps> disappear around their content. HTML comments go in every page. Code fences keep their
 * content and language, without the site's code-block options (title="…", wrap).
 */
export function toPlainMarkdown(body, { mdx = false, page = 'page' } = {}) {
  const input = body.replace(/\r\n/g, '\n').split('\n');
  const out = [];
  let fence = null; // the opening fence marker while inside a code block
  let level = 1; // the level of the last heading seen; the page title is the h1
  let inComment = false;

  for (let i = 0; i < input.length; i++) {
    const line = input[i];
    const fenceMatch = line.match(/^\s*(`{3,}|~{3,})/);
    if (fence) {
      out.push(line);
      if (fenceMatch && fenceMatch[1][0] === fence[0] && fenceMatch[1].length >= fence.length && line.trim() === fenceMatch[1]) fence = null;
      continue;
    }
    if (fenceMatch && !inComment) {
      fence = fenceMatch[1];
      // Keep the language and drop the code-block options the site uses (title="…", wrap).
      out.push(line.replace(/^(\s*(?:`{3,}|~{3,})[^\s`]*)\s.*$/, '$1'));
      continue;
    }

    // HTML comments, which may span lines.
    let text = line;
    if (inComment) {
      const end = text.indexOf('-->');
      if (end === -1) continue;
      text = text.slice(end + 3);
      inComment = false;
      if (text.trim() === '') continue;
    }
    text = text.replace(/<!--[\s\S]*?-->/g, '');
    if (text.includes('<!--')) {
      text = text.slice(0, text.indexOf('<!--'));
      inComment = true;
      if (text.trim() === '') continue;
    }
    if (text.trim() === '' && line.trim() !== '') continue;

    const heading = text.match(/^(#{1,6})\s/);
    if (heading) level = heading[1].length;

    if (!mdx) {
      out.push(text);
      continue;
    }

    if (/^import\s.+\sfrom\s+['"][^'"]+['"];?\s*$/.test(text)) continue;

    const tag = text.match(/^(\s*)<(\/?)([A-Z][A-Za-z]*)([^>]*?)(\/?)>\s*$/);
    if (!tag) {
      // A capitalized tag in running text, outside inline code, is a component this does not know.
      const stray = text.replace(/`[^`]*`/g, '').match(/<\/?[A-Z][A-Za-z]*[\s>/]/);
      if (stray) throw new Error(`${page}: no plain-Markdown form for "${stray[0].trim()}" (scripts/lib/llms.mjs)`);
      out.push(text);
      continue;
    }
    const [, indent, closing, name, attrs] = tag;
    if (DROPPED.includes(name)) continue;

    if ((name === 'Aside' || name === 'TabItem') && !closing) {
      // Collect the block up to its closing tag, then emit it dedented under its replacement.
      const inner = [];
      let depth = 1;
      let j = i + 1;
      for (; j < input.length; j++) {
        if (new RegExp(`^\\s*<${name}\\b`).test(input[j])) depth++;
        if (new RegExp(`^\\s*</${name}>\\s*$`).test(input[j]) && --depth === 0) break;
        inner.push(input[j]);
      }
      if (j === input.length) throw new Error(`${page}: <${name}> is never closed`);
      const content = toPlainMarkdown(dedent(trimBlank(inner)).join('\n'), { mdx, page }).split('\n');
      out.push('');
      if (name === 'Aside') {
        const kind = ASIDE_LABEL[attr(attrs, 'type') ?? 'note'] ?? 'Note';
        const title = attr(attrs, 'title');
        out.push(`${indent}> **${title ? `${kind}: ${title}` : kind}**`, `${indent}>`);
        for (const l of content) out.push(l === '' ? `${indent}>` : `${indent}> ${l}`);
      } else {
        const label = attr(attrs, 'label') ?? 'Tab';
        out.push(`${'#'.repeat(Math.min(level + 1, 6))} ${label}`, '');
        for (const l of content) out.push(l);
      }
      out.push('');
      i = j;
      continue;
    }
    throw new Error(`${page}: no plain-Markdown form for <${closing}${name}> (scripts/lib/llms.mjs)`);
  }

  // Collapse the blank lines that removed tags leave behind, outside code blocks.
  const collapsed = [];
  let inCode = false;
  for (const l of out) {
    if (/^\s*(`{3,}|~{3,})/.test(l)) inCode = !inCode;
    const blank = l.trim() === '';
    if (!inCode && blank && (collapsed.length === 0 || collapsed[collapsed.length - 1] === '')) continue;
    collapsed.push(!inCode && blank ? '' : l);
  }
  return trimBlank(collapsed).join('\n');
}

/**
 * Makes the site's own links absolute, so the Markdown works wherever it is read. A link to another
 * docs page goes to that page's Markdown copy; any other site path goes to the page on the site.
 */
export function absoluteLinks(markdown, { site, slugs }) {
  const origin = site.replace(/\/$/, '');
  const lines = markdown.split('\n');
  let fence = null;
  return lines
    .map((line) => {
      const fenceMatch = line.match(/^\s*(`{3,}|~{3,})/);
      if (fence) {
        if (fenceMatch && fenceMatch[1][0] === fence[0] && line.trim() === fenceMatch[1]) fence = null;
        return line;
      }
      if (fenceMatch) {
        fence = fenceMatch[1];
        return line;
      }
      // Leave inline code alone: split on code spans and rewrite only the text between them.
      return line
        .split(/(`[^`]*`)/)
        .map((part) =>
          part.startsWith('`')
            ? part
            : part.replace(/\]\((\/[^)\s]*)\)/g, (_, target) => {
                const [pathPart, hash] = target.split('#');
                const slug = pathPart.replace(/^\/|\/$/g, '');
                const url = slugs.has(slug) ? `${origin}${markdownPath(slug)}` : `${origin}${pathPart}`;
                return `](${url}${hash ? `#${hash}` : ''})`;
              }),
        )
        .join('');
    })
    .join('\n');
}

/** The docs pages in sidebar order, grouped as the sidebar groups them. */
export function orderedGroups(sidebar, entries) {
  const bySlug = new Map(entries.map((e) => [e.id, e]));
  return sidebar.map((group) => ({
    label: group.label,
    pages: group.items.map((slug) => {
      const entry = bySlug.get(slug);
      if (!entry) throw new Error(`the sidebar lists ${slug}, which is not a docs page`);
      return entry;
    }),
  }));
}

/** One page as standalone Markdown: its title, its address, its description as a quote, then its body. */
export function pageMarkdown(entry, { site, slugs }) {
  const mdx = /\.mdx$/.test(entry.filePath ?? '');
  const body = toPlainMarkdown(entry.body ?? '', { mdx, page: entry.filePath ?? entry.id });
  const parts = [`# ${entry.data.title}`, '', `URL: ${site.replace(/\/$/, '')}/${entry.id}/`, ''];
  if (entry.data.description) parts.push(`> ${entry.data.description}`, '');
  // An article's byline: who wrote it and the day it was published.
  if (entry.data.date) {
    const day = new Date(entry.data.date).toISOString().slice(0, 10);
    parts.push(entry.data.author ? `${entry.data.author}, ${day}` : day, '');
  }
  parts.push(absoluteLinks(body, { site, slugs }), '');
  return parts.join('\n');
}

// Groups that are about the project rather than about using ReFHIR. llms.txt lists them under
// "Optional", which the format reserves for pages a reader may skip; llms-full.txt leaves them out.
const OPTIONAL_GROUPS = new Set(['Project']);
// Pages llms.txt lists but llms-full.txt leaves out: the changelog is release history, longer than
// any other page, and not needed to use the server.
const NOT_IN_FULL = new Set(['docs/reference/changelog']);

export const SUMMARY =
  'ReFHIR is an open-source FHIR R4 server that keeps query results live. A client runs a FHIR query and stays subscribed to its result; when a committed write changes what the query returns, the server re-runs it and sends the new result.';

const DETAILS = `It is also an ordinary FHIR server: REST, search, batch and transactions, with SMART on FHIR for authorization. Live queries run over a WebSocket at \`/sync\`. The TypeScript client is \`@haihq/refhir\` on npm (\`dial\`, \`client.search\`; the React hook \`useFHIRQuery\` is in \`@haihq/refhir/react\`), and the Go client is \`github.com/hai-hq/refhir/clients/go\`.

Start a server with Docker: \`docker run --rm -p 8080:8080 -v refhir-data:/data ghcr.io/hai-hq/refhir:latest\`. It is ready when \`curl -s http://127.0.0.1:8080/readyz\` prints \`{"status":"ok"}\`.

Each link below is the page's Markdown. The same page as HTML is at that address without \`.md\`, with a trailing slash. Every page in one file: `;

/** /llms.txt, in the llmstxt.org format. */
export function llmsTxt(groups, { site }) {
  const origin = site.replace(/\/$/, '');
  const lines = ['# ReFHIR', '', `> ${SUMMARY}`, '', `${DETAILS}${origin}/llms-full.txt`, ''];
  const section = (heading, pages) => {
    lines.push(`## ${heading}`, '');
    for (const page of pages) {
      const description = page.data.description ? `: ${page.data.description}` : '';
      lines.push(`- [${page.data.title}](${origin}${markdownPath(page.id)})${description}`);
    }
    lines.push('');
  };
  for (const group of groups) if (!OPTIONAL_GROUPS.has(group.label)) section(group.label, group.pages);
  const optional = groups.filter((g) => OPTIONAL_GROUPS.has(g.label)).flatMap((g) => g.pages);
  if (optional.length) section('Optional', optional);
  return lines.join('\n');
}

/** /llms-full.txt: every page's Markdown in sidebar order, each under its title and address. */
export function llmsFullTxt(groups, { site, slugs }) {
  const origin = site.replace(/\/$/, '');
  const skipped = [...NOT_IN_FULL].map((slug) => `${origin}${markdownPath(slug)}`).join(', ');
  const parts = [
    `# ReFHIR documentation`,
    '',
    `> ${SUMMARY}`,
    '',
    `Every docs page, in the order of the site's sidebar. The index is ${origin}/llms.txt; the changelog is not included here and is at ${skipped}.`,
    '',
  ];
  for (const group of groups) {
    if (OPTIONAL_GROUPS.has(group.label)) continue;
    for (const page of group.pages) {
      if (NOT_IN_FULL.has(page.id)) continue;
      parts.push('---', '', pageMarkdown(page, { site, slugs }));
    }
  }
  return parts.join('\n');
}

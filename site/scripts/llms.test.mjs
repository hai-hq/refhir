import assert from 'node:assert/strict';
import { test } from 'node:test';

import { absoluteLinks, llmsTxt, orderedGroups, pageMarkdown, toPlainMarkdown } from './lib/llms.mjs';

const SITE = 'https://refhir.haihq.org';

test('MDX imports go and Tabs become headings one level below the section', () => {
  const mdx = `import { Aside, TabItem, Tabs } from '@astrojs/starlight/components';

## Run it

<Tabs>
  <TabItem label="Docker">
    Pull the image.

    \`\`\`bash
    docker run x
    \`\`\`
  </TabItem>
  <TabItem label="From source">
    Build it.
  </TabItem>
</Tabs>`;
  assert.equal(
    toPlainMarkdown(mdx, { mdx: true }),
    `## Run it

### Docker

Pull the image.

\`\`\`bash
docker run x
\`\`\`

### From source

Build it.`,
  );
});

test('an Aside becomes a titled quote, also inside a list item', () => {
  const mdx = `<Steps>

1. **Register.**

   <Aside type="caution" title="aud is required">
     Send \`aud\`.

     Always.
   </Aside>

</Steps>`;
  assert.equal(
    toPlainMarkdown(mdx, { mdx: true }),
    `1. **Register.**

   > **Caution: aud is required**
   >
   > Send \`aud\`.
   >
   > Always.`,
  );
});

test('code fences and inline code are left alone', () => {
  const mdx = '```mdx\n<Tabs>\n\n\nimport x from "y";\n```\n\nUse `<Tabs>` here.';
  assert.equal(toPlainMarkdown(mdx, { mdx: true }), mdx);
});

test('code-block options are dropped from the fence, the language stays', () => {
  assert.equal(toPlainMarkdown('```text title="Prompt" wrap\nhi\n```'), '```text\nhi\n```');
});

test('an unknown component fails instead of leaking into the Markdown', () => {
  assert.throws(() => toPlainMarkdown('<Card title="x">\nhi\n</Card>', { mdx: true, page: 'p.mdx' }), /p\.mdx.*Card/);
  assert.throws(() => toPlainMarkdown('Some <Badge text="new" /> text', { mdx: true }), /Badge/);
});

test('HTML comments are removed from Markdown pages', () => {
  assert.equal(toPlainMarkdown('<!-- generated -->\n\nText <!-- x --> here.\n<!--\nmany\n-->\nEnd'), 'Text  here.\nEnd');
});

test('site links become absolute, docs links point at the Markdown copy', () => {
  const slugs = new Set(['docs/install']);
  assert.equal(
    absoluteLinks('[a](/docs/install/#docker) [b](/brand/x.svg) [c](https://e.org/) `[d](/docs/install/)`', { site: SITE, slugs }),
    `[a](${SITE}/docs/install.md#docker) [b](${SITE}/brand/x.svg) [c](https://e.org/) \`[d](/docs/install/)\``,
  );
});

const entry = (id, title, extra = {}) => ({ id, data: { title, description: `${title} page.` }, body: 'Body.', filePath: `${id}.md`, ...extra });

test('llms.txt follows the sidebar and puts the project pages under Optional', () => {
  const sidebar = [
    { label: 'Start', items: ['docs/install'] },
    { label: 'Project', items: ['docs/project/security'] },
  ];
  const groups = orderedGroups(sidebar, [entry('docs/project/security', 'Security'), entry('docs/install', 'Install')]);
  const text = llmsTxt(groups, { site: SITE });
  assert.match(text, /^# ReFHIR\n\n> /);
  assert.match(text, /## Start\n\n- \[Install\]\(https:\/\/refhir\.haihq\.org\/docs\/install\.md\): Install page\.\n/);
  assert.match(text, /## Optional\n\n- \[Security\]/);
  assert.doesNotMatch(text, /## Project/);
});

test('the sidebar may not name a page that does not exist', () => {
  assert.throws(() => orderedGroups([{ label: 'Start', items: ['docs/missing'] }], []), /docs\/missing/);
});

test('a page starts with its title, address and description', () => {
  const md = pageMarkdown(entry('docs/install', 'Install'), { site: SITE, slugs: new Set() });
  assert.equal(md, `# Install\n\nURL: ${SITE}/docs/install/\n\n> Install page.\n\nBody.\n`);
});

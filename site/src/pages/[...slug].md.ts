// The Markdown copy of each docs page, served beside it: /docs/tutorial.md next to /docs/tutorial/.
import type { APIRoute, GetStaticPaths } from 'astro';

import { pageMarkdown } from '../../scripts/lib/llms.mjs';
import { docsInSidebarOrder, plainText, SITE } from '../lib/llms';

export const getStaticPaths = (async () => {
  const { groups, slugs } = await docsInSidebarOrder();
  return groups.flatMap((group) =>
    group.pages.map((page) => ({ params: { slug: page.id }, props: { markdown: pageMarkdown(page, { site: SITE, slugs }) } })),
  );
}) satisfies GetStaticPaths;

export const GET: APIRoute = ({ props }) => plainText(props.markdown);

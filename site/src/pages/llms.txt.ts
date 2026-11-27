// /llms.txt: an index of the docs for AI coding assistants (llmstxt.org).
import type { APIRoute } from 'astro';

import { llmsTxt } from '../../scripts/lib/llms.mjs';
import { docsInSidebarOrder, plainText, SITE } from '../lib/llms';

export const GET: APIRoute = async () => {
  const { groups } = await docsInSidebarOrder();
  return plainText(llmsTxt(groups, { site: SITE }));
};

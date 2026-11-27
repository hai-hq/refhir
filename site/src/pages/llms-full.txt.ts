// /llms-full.txt: every docs page as Markdown, in sidebar order, in one file.
import type { APIRoute } from 'astro';

import { llmsFullTxt } from '../../scripts/lib/llms.mjs';
import { docsInSidebarOrder, plainText, SITE } from '../lib/llms';

export const GET: APIRoute = async () => {
  const { groups, slugs } = await docsInSidebarOrder();
  return plainText(llmsFullTxt(groups, { site: SITE, slugs }));
};

// The docs collection, in sidebar order, for the plain-Markdown endpoints (llms.txt, llms-full.txt
// and the .md copy of each page). The Markdown itself is made by scripts/lib/llms.mjs.
import { type CollectionEntry, getCollection } from 'astro:content';

import { sidebar } from '../../docs-manifest.mjs';
import { orderedGroups } from '../../scripts/lib/llms.mjs';

export const SITE = import.meta.env.SITE;

export type DocsGroup = { label: string; pages: CollectionEntry<'docs'>[] };

export async function docsInSidebarOrder() {
  const entries = await getCollection('docs');
  const groups: DocsGroup[] = orderedGroups(sidebar, entries);
  const slugs = new Set(groups.flatMap((g) => g.pages.map((p) => p.id)));
  return { groups, slugs };
}

export const plainText = (body: string, type = 'text/plain; charset=utf-8') =>
  new Response(body, { headers: { 'Content-Type': type } });

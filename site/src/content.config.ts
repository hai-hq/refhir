import { defineCollection } from 'astro:content';
import { docsLoader, i18nLoader } from '@astrojs/starlight/loaders';
import { docsSchema, i18nSchema } from '@astrojs/starlight/schema';

export const collections = {
  docs: defineCollection({ loader: docsLoader(), schema: docsSchema() }),
  // The site is English only; Starlight warns when this collection is missing.
  i18n: defineCollection({ loader: i18nLoader(), schema: i18nSchema() }),
};

import { defineCollection } from 'astro:content';
import { z } from 'astro/zod';
import { docsLoader, i18nLoader } from '@astrojs/starlight/loaders';
import { docsSchema, i18nSchema } from '@astrojs/starlight/schema';

export const collections = {
  docs: defineCollection({
    loader: docsLoader(),
    // Articles (src/content/docs/docs/articles/) carry the day they were published and who wrote
    // them; src/components/PageTitle.astro shows both under the title.
    schema: docsSchema({
      extend: z.object({
        date: z.coerce.date().optional(),
        author: z.string().optional(),
      }),
    }),
  }),
  // The site is English only; Starlight warns when this collection is missing.
  i18n: defineCollection({ loader: i18nLoader(), schema: i18nSchema() }),
};

// @ts-check
import starlight from '@astrojs/starlight';
import { defineConfig } from 'astro/config';
import starlightLinksValidator from 'starlight-links-validator';

import { sidebar } from './docs-manifest.mjs';

const SITE = 'https://refhir.haihq.org';

// https://astro.build/config
export default defineConfig({
  site: SITE,
  trailingSlash: 'always',
  // Astro 7's default ('jsx') drops the space between text and an inline element on the next line.
  compressHTML: true,
  // The docs have no index page of their own; send /docs/ to the first page.
  redirects: { '/docs': '/docs/introduction/' },
  integrations: [
    starlight({
      title: 'ReFHIR',
      description:
        'ReFHIR is an open-source FHIR R4 server that keeps query results live. Run a query, stay subscribed, and get the new result when a commit changes it.',
      logo: {
        light: './src/assets/brand/refhir-logo.svg',
        dark: './src/assets/brand/refhir-logo-reversed.svg',
        alt: 'ReFHIR',
        replacesTitle: true,
      },
      favicon: '/brand/refhir-icon.svg',
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/hai-hq/refhir' }],
      // Pages written in site/ are edited here; synced pages carry their own editUrl.
      editLink: { baseUrl: 'https://github.com/hai-hq/refhir/edit/main/site/' },
      customCss: [
        '@fontsource-variable/inter/index.css',
        '@fontsource/ibm-plex-mono/500.css',
        './src/styles/brand.css',
      ],
      sidebar,
      // The docs use ```env fences; Shiki calls that language dotenv.
      expressiveCode: { shiki: { langAlias: { env: 'dotenv' } } },
      head: [
        { tag: 'meta', attrs: { property: 'og:image', content: `${SITE}/brand/og-image.png` } },
        { tag: 'meta', attrs: { property: 'og:image:type', content: 'image/png' } },
        { tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
        { tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
        {
          tag: 'meta',
          attrs: {
            property: 'og:image:alt',
            content: 'ReFHIR: FHIR data that updates itself. An open-source FHIR server with live queries.',
          },
        },
        { tag: 'meta', attrs: { property: 'og:locale', content: 'en_US' } },
        { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
        { tag: 'meta', attrs: { name: 'twitter:image', content: `${SITE}/brand/og-image.png` } },
        {
          tag: 'meta',
          attrs: {
            name: 'twitter:image:alt',
            content: 'ReFHIR: FHIR data that updates itself. An open-source FHIR server with live queries.',
          },
        },
        { tag: 'link', attrs: { rel: 'icon', href: '/favicon.ico', sizes: '48x48' } },
        { tag: 'link', attrs: { rel: 'apple-touch-icon', href: '/apple-touch-icon.png' } },
      ],
      // Docs show commands against http://127.0.0.1:8080, so local links are expected. Relative
      // links and bad anchors still fail the build.
      plugins: [starlightLinksValidator({ errorOnLocalLinks: false })],
    }),
  ],
});

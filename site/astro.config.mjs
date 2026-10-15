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
      head: [
        { tag: 'meta', attrs: { property: 'og:image', content: `${SITE}/brand/social-preview.png` } },
        { tag: 'meta', attrs: { property: 'og:image:width', content: '1280' } },
        { tag: 'meta', attrs: { property: 'og:image:height', content: '640' } },
        { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
        { tag: 'meta', attrs: { name: 'twitter:image', content: `${SITE}/brand/social-preview.png` } },
        { tag: 'link', attrs: { rel: 'apple-touch-icon', href: '/brand/refhir-icon-512.png' } },
      ],
      // Docs show commands against http://127.0.0.1:8080, so local links are expected. Relative
      // links and bad anchors still fail the build.
      plugins: [starlightLinksValidator({ errorOnLocalLinks: false })],
    }),
  ],
});

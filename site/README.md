# ReFHIR website

The landing page and documentation for ReFHIR, built with [Astro](https://astro.build) and
[Starlight](https://starlight.astro.build). It is fully static: `npm run build` writes plain files
to `dist/`, and there is no server.

```bash
cd site
npm ci
npm run dev        # http://localhost:4321, with the docs synced first
npm run build      # sync, type-check, build, and validate every internal link
npm test           # tests for the sync step and the docs manifest
```

Node 22.12 or newer.

## Where content lives

- **Existing docs stay where they are.** `docs/`, `deploy/`, `clients/*/README.md` and the root
  Markdown files remain the source of truth and keep working on GitHub. `docs-manifest.mjs` lists
  the ones the site publishes, and `scripts/sync-content.mjs` copies them into
  `src/content/docs/synced/` (gitignored) with front matter added and links rewritten. To change a
  synced page, edit the original file; each page's "Edit page" link goes to it.
- **Pages that exist only on the website** are real files under `src/content/docs/docs/`.
- **The landing page** is `src/pages/index.astro`.
- **Brand files** come from `docs/brand/`; the sync step copies them to `public/brand/` and
  `src/assets/brand/` (gitignored).

## Adding a page

1. A site-only page: add `src/content/docs/docs/<name>.md` with a `title`, then list its slug
   (`docs/<name>`) in `sidebar` in `docs-manifest.mjs`.
2. A page from an existing document: add an entry to `synced` in `docs-manifest.mjs` and list its
   slug in `sidebar`. The manifest is an explicit allowlist, and only files git tracks can be
   listed, so private or ignored files are never published.
3. `npm test` checks that every synced page is in the sidebar and every sidebar slug has a page.
   The build fails on any link that does not resolve.

## Deploying

Cloudflare Pages builds from `main` with root directory `site`, build command
`npm ci && npm run build`, and output directory `dist`. `.node-version` selects Node 22.
`public/_headers` sets the security headers and marks `*.pages.dev` previews `noindex`.

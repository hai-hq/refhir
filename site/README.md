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

## Docs that describe behaviour that is about to change

Four product fixes were written in other branches and are not merged here. Some pages state today's
behaviour as a fact. When a fix merges, update its pages in the same change:

- **A bare id in a reference search** (`patient=123` finds nothing today) and **a malformed search
  value** (`birthdate=notadate` answers `200` today): `docs/fhir-api.md`, under Search, and
  `src/content/docs/docs/operate/troubleshooting.md`, where it describes malformed values.
- **Subscription delivery** (the server never changes `status`; `channel.header`, `channel.payload`
  and `end` are ignored; eight attempts from memory): `src/content/docs/docs/guides/subscriptions.md`.
- **Conflicting storage flags** (`--memory` with `--sqlite` or `--postgres`): the flag table in
  `docs/configuration.md` already says they cannot be combined, so check that it still matches.

## Adding a page

1. A site-only page: add `src/content/docs/docs/<name>.md` with a `title`, then list its slug
   (`docs/<name>`) in `sidebar` in `docs-manifest.mjs`.
2. A page from an existing document: add an entry to `synced` in `docs-manifest.mjs` and list its
   slug in `sidebar`. The manifest is an explicit allowlist, and only files git tracks can be
   listed, so private or ignored files are never published.
3. `npm test` checks that every synced page is in the sidebar and every sidebar slug has a page.
   The build fails on any link that does not resolve.

## Deploying

The site is served at `refhir.haihq.org` from Cloudflare Pages, using its Git integration. Nothing in
this repository holds a Cloudflare token: the project is configured in the Cloudflare dashboard, and
the `site.yml` workflow only builds and checks the site on pull requests.

### Set it up once

1. In the Cloudflare dashboard, create a Pages project and connect it to the GitHub repository.
2. Configure the build:

   | Setting | Value |
   |---|---|
   | Production branch | `main` |
   | Root directory | `site` |
   | Build command | `npm ci && npm run build` |
   | Build output directory | `dist` |
   | Environment variable `NODE_VERSION` | `22` (Pages also reads `site/.node-version`) |

   Pages clones the whole repository, which the build needs: it reads `docs/`, `deploy/`,
   `clients/` and the root Markdown files from outside `site/`, and it uses `git ls-files` to know
   which files it may publish.
3. Add the custom domain `refhir.haihq.org` to the project. Then, in HAIHQ's DNS, create a `CNAME`
   record for `refhir` that points at the project's `<name>.pages.dev` address.
4. Pull requests get preview deployments on `*.pages.dev`. `public/_headers` marks those `noindex`.

### Check it after the first deploy

```bash
curl -sI https://refhir.haihq.org/ | head -n 12      # security headers from public/_headers
curl -s  https://refhir.haihq.org/robots.txt
curl -sI https://refhir.haihq.org/sitemap-index.xml | head -n 1
curl -sI https://<name>.pages.dev/ | grep -i x-robots-tag   # expect: noindex
```

Open `/docs/install/`, search for a page with the search box, and switch between light and dark.

### Go-live changes in the repository

Nothing in the repository points at the site until the DNS record is live, so a merge cannot publish a
dead link. When it is live, make these three edits in one change:

- Root `README.md`: add `**Documentation and website:** [refhir.haihq.org](https://refhir.haihq.org)`
  under the badges.
- `.github/ISSUE_TEMPLATE/config.yml`: point the Documentation link at
  `https://refhir.haihq.org/docs/introduction/`.
- `clients/ts/package.json`: set `homepage` to `https://refhir.haihq.org/docs/sdks/typescript/`.

Two things outside the repository finish the job: the repository's website field on GitHub, and a link
from `haihq.org/refhir` to this site.

### If a build fails

- **A link error** names the page and the link. Fix the original document, because synced pages are
  generated.
- **`sync-content` fails** when a listed file is not tracked by git, or a link points at a file that
  does not exist.
- **`check-built` fails** when the landing page breaks a promise: it loads something from another
  origin, uses a claim the project cannot back up, or grows past its JavaScript budget. It also
  checks search and sharing: every page has a title, description, canonical url, one h1 and share
  tags with no duplicates; the structured data matches `Cargo.toml`; the share image is the 1200 by
  630 file the tags declare; robots.txt and the sitemap are right. See `CLAIMS.md`.
- **The share image** is `docs/brand/og-image.png`. To change it, redesign it at 1200 by 630 and keep
  it under 600 KB; the check fails otherwise.
- **A Node error** on Cloudflare usually means the project's Node version is older than 22.12. Set
  `NODE_VERSION`.

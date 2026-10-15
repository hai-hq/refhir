// The documentation the website publishes.
//
// `synced` is an explicit allowlist of canonical repository documents. It is never a glob: a file
// is published only when it is listed here (and git tracks it, which scripts/sync-content.mjs
// enforces). Each entry becomes a page at /<slug>/. Pages that exist only on the website are real
// files under src/content/docs/docs/ and are named in `sidebar` by their slug.
//
// entry: { source, slug, label?, title?, description?, preface? }
//   source       repository path of the canonical Markdown file
//   slug         URL path, always under docs/
//   label        sidebar text, when the H1 is too long or unclear
//   title        only needed when the source has no Markdown H1
//   description  page description for search and social previews
//   preface      Starlight Markdown inserted above the body (for example a :::caution)
export const synced = [
  {
    source: 'docs/getting-started.md',
    slug: 'docs/getting-started',
    description: 'Run ReFHIR with Docker or from source, and try it with curl.',
  },
  {
    source: 'docs/fhir-api.md',
    slug: 'docs/guides/fhir-api',
    label: 'FHIR API',
    description: 'What the server supports: interactions, search, operations, resource ids and terminology.',
  },
  {
    source: 'docs/smart.md',
    slug: 'docs/guides/smart',
    label: 'SMART on FHIR',
    description: 'Authorization, sign-in and scopes with SMART App Launch.',
  },
  {
    source: 'docs/design.md',
    slug: 'docs/concepts/design',
    description: 'Why ReFHIR exists, how a write becomes a live-query update, and how the engine is built.',
  },
];

// Sidebar groups, in display order. Each item is a page slug.
export const sidebar = [
  { label: 'Start', items: ['docs/introduction', 'docs/getting-started'] },
  { label: 'Concepts', items: ['docs/concepts/design'] },
  { label: 'Guides', items: ['docs/guides/fhir-api', 'docs/guides/smart'] },
];

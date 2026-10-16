// The documentation the website publishes.
//
// `synced` is an explicit allowlist of canonical repository documents. It is never a glob: a file
// is published only when it is listed here (and git tracks it, which scripts/sync-content.mjs
// enforces). Each entry becomes a page at /<slug>/. Pages that exist only on the website are real
// files under src/content/docs/docs/ and are named in `sidebar` by their slug.
//
// entry: { source, slug, label?, title?, description? }
//   source       repository path of the canonical Markdown file
//   slug         URL path, always under docs/
//   label        sidebar text, when the H1 is too long or unclear
//   title        only needed when the source has no Markdown H1
//   description  page description for search and social previews

export const synced = [
  // Start
  {
    source: 'docs/getting-started.md',
    slug: 'docs/getting-started',
    description: 'Run ReFHIR with Docker or from source, and try it with curl.',
  },

  // Concepts

  // Guides
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
    source: 'docs/governance.md',
    slug: 'docs/guides/governance',
    label: 'Governance',
    description: 'Metering, request logs, an audit trail, rate limits and quotas: what each does and how to set it.',
  },
  {
    source: 'fixtures/mock-fhir/README.md',
    slug: 'docs/guides/test-data',
    label: 'Test data',
    description: 'The mock FHIR data used by the seed command, and how to load your own.',
  },

  // SDKs
  {
    source: 'clients/ts/README.md',
    slug: 'docs/sdks/typescript',
    label: 'TypeScript',
    description: 'The TypeScript client: dial, live queries, the React hook and read-your-writes.',
  },
  {
    source: 'clients/go/README.md',
    slug: 'docs/sdks/go',
    label: 'Go',
    description: 'The Go client: live queries, REST helpers, reconnects and the demo-data seed command.',
  },
  {
    source: 'docs/sync-protocol.md',
    slug: 'docs/sdks/sync-protocol',
    label: 'The /sync protocol',
    description: 'The /sync WebSocket protocol, for anyone writing a client in another language.',
  },

  // Operate
  {
    source: 'deploy/production.md',
    slug: 'docs/operate/production',
    description: 'Postgres, secrets, SMART sign-in, Helm and a reverse proxy: what to decide before you run ReFHIR for real.',
  },
  {
    source: 'docs/operations.md',
    slug: 'docs/operate/operations',
    description: 'Health probes, Postgres and TLS settings, and how export jobs run, for anyone operating a ReFHIR server.',
  },
  {
    source: 'docs/clustering.md',
    slug: 'docs/operate/clustering',
    label: 'Several processes',
    description: 'Run several processes on one database: a primary, standbys that take over, and read replicas.',
  },
  {
    source: 'deploy/cluster/README.md',
    slug: 'docs/operate/compose-cluster',
    label: 'Compose cluster',
    description: 'A Docker Compose cluster with two writers, two replicas and an HAProxy load balancer, plus a failover test.',
  },
  {
    source: 'deploy/helm/refhir/README.md',
    slug: 'docs/operate/helm',
    label: 'Helm chart',
    description: 'Install and configure ReFHIR on Kubernetes with the Helm chart.',
  },
  {
    source: 'docs/migrations.md',
    slug: 'docs/operate/migrations',
    description: 'Forward-only schema migrations, how to upgrade safely, and how to add one.',
  },

  // Reference
  {
    source: 'docs/configuration.md',
    slug: 'docs/reference/configuration',
    description: 'The configuration file, its search order, and every command-line flag.',
  },
  {
    source: 'docs/smart-compliance.md',
    slug: 'docs/reference/smart-support',
    label: 'SMART support',
    description: 'What ReFHIR implements of SMART App Launch 2.2, how to configure sign-in, and what to know about it.',
  },
  {
    source: 'CHANGELOG.md',
    slug: 'docs/reference/changelog',
    description: 'The ReFHIR changelog: the notable changes in each release.',
  },

  // Project
  {
    source: 'CONTRIBUTING.md',
    slug: 'docs/project/contributing',
    label: 'Contributing',
    description: 'How to report bugs, propose changes and send a pull request.',
  },
  {
    source: 'SECURITY.md',
    slug: 'docs/project/security',
    label: 'Security policy',
    description: 'Supported versions and how to report a vulnerability privately.',
  },
  {
    source: 'CODE_OF_CONDUCT.md',
    slug: 'docs/project/code-of-conduct',
    label: 'Code of conduct',
    description: 'The Contributor Covenant that applies to everyone taking part.',
  },
];

// Pages built from the source by scripts/lib/reference.mjs: the command-line help and the example
// configuration file. They are listed here so the sidebar and its tests know they exist.
export const generated = [{ slug: 'docs/reference/cli' }, { slug: 'docs/reference/config-file' }];

// Sidebar groups, in display order. Each item is a page slug.
export const sidebar = [
  { label: 'Start', items: ['docs/introduction', 'docs/install', 'docs/getting-started', 'docs/tutorial'] },
  {
    label: 'Concepts',
    items: [
      'docs/concepts/how-it-works',
      'docs/concepts/agents',
      'docs/concepts/vs-subscriptions',
      'docs/concepts/glossary',
    ],
  },
  {
    label: 'Guides',
    items: [
      'docs/guides/fhir-api',
      'docs/guides/subscriptions',
      'docs/guides/smart',
      'docs/guides/register-smart-app',
      'docs/guides/governance',
      'docs/guides/test-data',
    ],
  },
  {
    label: 'SDKs',
    items: ['docs/sdks/typescript', 'docs/sdks/react', 'docs/sdks/go', 'docs/sdks/sync-protocol'],
  },
  {
    label: 'Operate',
    items: [
      'docs/operate/production',
      'docs/operate/operations',
      'docs/operate/clustering',
      'docs/operate/compose-cluster',
      'docs/operate/helm',
      'docs/operate/migrations',
      'docs/operate/troubleshooting',
    ],
  },
  {
    label: 'Reference',
    items: [
      'docs/reference/configuration',
      'docs/reference/cli',
      'docs/reference/config-file',
      'docs/reference/environment-variables',
      'docs/reference/smart-support',
      'docs/reference/changelog',
    ],
  },
  {
    label: 'Project',
    collapsed: true,
    items: [
      'docs/project/contributing',
      'docs/project/security',
      'docs/project/code-of-conduct',
    ],
  },
];

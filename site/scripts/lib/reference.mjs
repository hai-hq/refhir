// Reference pages that are generated from the repository when the site builds, so they cannot drift
// from what the code and the example configuration say.
//
//   Command line        from the help text in crates/refhir/src/main.rs and migrate.rs
//   Configuration file  from refhir.toml.example, shown whole
//
// Each function returns { slug, title, description, source, body }. `source` is the repository file
// the page comes from, which is where its "Edit page" link goes.
import { rustStringAfter } from './rust-string.mjs';

export const MAIN_RS = 'crates/refhir/src/main.rs';
export const MIGRATE_RS = 'crates/refhir/src/migrate.rs';
export const EXAMPLE_TOML = 'refhir.toml.example';

/** `refhir --help`, exactly as the binary prints it. */
export function serverHelp(mainRs) {
  const marker = mainRs.indexOf('fn print_help()');
  if (marker === -1) throw new Error(`${MAIN_RS}: no fn print_help()`);
  return rustStringAfter(mainRs.slice(marker), 'println!(', { format: true });
}

/** `refhir migrate --help`. */
export function migrateHelp(migrateRs) {
  return rustStringAfter(migrateRs, 'pub const HELP: &str =');
}

export function cliPage({ mainRs, migrateRs }) {
  const body = `This page is generated from the help text in the source when the site is built, so it is what
\`refhir --help\` prints. Every flag also has a place in the configuration file, and a flag overrides
the file. [Configuration](/docs/reference/configuration/) explains each flag in a table, and
[Environment variables](/docs/reference/environment-variables/) lists the variables the server reads.

## refhir

\`\`\`text
${serverHelp(mainRs)}
\`\`\`

## refhir migrate

\`\`\`text
${migrateHelp(migrateRs)}
\`\`\`

See [Schema migrations](/docs/operate/migrations/) for what the exit codes mean in an upgrade.
`;
  return {
    slug: 'docs/reference/cli',
    title: 'Command line',
    description: 'Every flag of the refhir command and of refhir migrate, generated from the source.',
    source: MAIN_RS,
    body,
  };
}

export function configFilePage({ exampleToml }) {
  const body = `This is \`${EXAMPLE_TOML}\`, shown whole and generated from the file when the site is built. It lists
every setting, with a comment on each. Treat the values as examples, not as defaults: where a
default differs, the comment says so, and a setting left out of your file takes the server's own
default.

Copy it to \`refhir.toml\`, or mount it at \`/etc/refhir/refhir.toml\`. The server looks for
\`./refhir.toml\`, then \`/etc/refhir/refhir.toml\`, or the path in \`REFHIR_CONFIG\`. Command-line flags
override the file; see [Command line](/docs/reference/cli/) and
[Configuration](/docs/reference/configuration/).

\`\`\`toml title="refhir.toml.example"
${exampleToml.replace(/\s+$/, '')}
\`\`\`
`;
  return {
    slug: 'docs/reference/config-file',
    title: 'Configuration file',
    description: 'Every setting in the example configuration file, with its comments, generated from the file.',
    source: EXAMPLE_TOML,
    body,
  };
}

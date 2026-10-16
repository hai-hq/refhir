import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';

import { cliPage, configFilePage, migrateHelp, serverHelp } from './lib/reference.mjs';
import { repoRoot, siteRoot } from './lib/repo.mjs';
import { rustStringAfter } from './lib/rust-string.mjs';

const read = (file) => fs.readFileSync(path.join(repoRoot, file), 'utf8');

test('a Rust string literal is read with its escapes, continuations and format braces', () => {
  const src = 'const X: &str = "a\\n\\\n    b \\"q\\" \\\\ {{x}}\\n\\\n   c";';
  assert.equal(rustStringAfter(src, 'const X', { format: true }), 'a\nb "q" \\ {x}\nc');
  assert.equal(rustStringAfter(src, 'const X'), 'a\nb "q" \\ {{x}}\nc');
});

test('real line breaks inside a literal are kept, and a leading continuation is dropped', () => {
  const src = 'pub const HELP: &str = "\\\nfirst\n\nsecond";';
  assert.equal(rustStringAfter(src, 'HELP'), 'first\n\nsecond');
});

test('an escape the reader does not know fails instead of rendering wrong', () => {
  assert.throws(() => rustStringAfter('X = "bad \\t tab";', 'X'), /unsupported escape/);
  assert.throws(() => rustStringAfter('X = "never closed', 'X'), /unterminated/);
  assert.throws(() => rustStringAfter('nothing here', 'X'), /not found/);
});

test('the server help comes out of main.rs whole and clean', () => {
  const help = serverHelp(read('crates/refhir/src/main.rs'));
  assert.match(help, /^refhir \[--config path\]/);
  assert.match(help, /refhir --version \| -V/);
  assert.match(help, /--update-as-create/);
  assert.doesNotMatch(help, /\\n|\{\{|\}\}/);
  assert.ok(help.split('\n').length > 30, 'the help text is suspiciously short');
});

test('the migrate help comes out of migrate.rs', () => {
  const help = migrateHelp(read('crates/refhir/src/migrate.rs'));
  assert.match(help, /^refhir migrate --status \| --dry-run/);
  assert.match(help, /Exit codes: 0 /);
});

// When a debug or release binary is built, the generated text must be exactly what it prints. In CI
// there is no binary, so this is skipped; it runs for anyone who has built the server.
const binary = ['target/debug/refhir', 'target/release/refhir'].map((p) => path.join(repoRoot, p)).find((p) => fs.existsSync(p));
test('the generated help equals what the binary prints', { skip: binary ? false : 'no built refhir binary' }, () => {
  const printed = execFileSync(binary, ['--help'], { encoding: 'utf8' });
  assert.equal(`${serverHelp(read('crates/refhir/src/main.rs'))}\n`, printed);
  const migrate = execFileSync(binary, ['migrate', '--help'], { encoding: 'utf8' });
  assert.equal(`${migrateHelp(read('crates/refhir/src/migrate.rs'))}\n`, migrate);
});

test('the command-line page holds both helps and the config page holds the example whole', () => {
  const cli = cliPage({ mainRs: read('crates/refhir/src/main.rs'), migrateRs: read('crates/refhir/src/migrate.rs') });
  assert.match(cli.body, /## refhir migrate/);
  assert.ok(cli.body.includes('Exit codes: 0'));
  const toml = read('refhir.toml.example');
  const config = configFilePage({ exampleToml: toml });
  assert.ok(config.body.includes(toml.trim()), 'the example file is not shown whole');
  assert.doesNotMatch(toml, /```/, 'a code fence in the example would break the page');
});

// The page lists every variable the server reads, so one cannot be added without being documented.
// These are read only by the project's own test suites, so the page leaves them out.
const TEST_ONLY = new Set([
  'REFHIR_TEST_POSTGRES_URL',
  'REFHIR_REQUIRE_POSTGRES',
  'REFHIR_TLS_POSTGRES_ADDR',
  'REFHIR_TLS_POSTGRES_DIR',
  'REFHIR_REQUIRE_POSTGRES_TLS',
]);
test('every REFHIR_ variable the code reads is on the environment variables page', () => {
  const page = fs.readFileSync(path.join(siteRoot, 'src/content/docs/docs/reference/environment-variables.md'), 'utf8');
  const out = execFileSync('git', ['grep', '-hoE', '"REFHIR_[A-Z0-9_]+"', '--', 'crates/**/*.rs'], {
    cwd: repoRoot,
    encoding: 'utf8',
  });
  const names = [...new Set(out.split('\n').filter(Boolean).map((s) => s.replace(/"/g, '')))].filter(
    (name) => !TEST_ONLY.has(name),
  );
  assert.ok(names.length > 15, `only found ${names.length} variables; the search is probably broken`);
  const missing = names.filter((name) => !page.includes(`\`${name}\``));
  assert.deepEqual(missing, [], `these variables are read by the code but not on the page: ${missing.join(', ')}`);
});

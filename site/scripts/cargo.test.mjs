import assert from 'node:assert/strict';
import test from 'node:test';

import { workspaceVersionFrom } from './lib/cargo.mjs';

test('the version is found whatever sits above it in [workspace.package]', () => {
  const toml = `[workspace]
members = ["a"]

[workspace.package]
authors = ["x", "y"]
keywords = [
  "fhir",
]
version = "1.2.3"

[workspace.dependencies]
version = "9"
`;
  assert.equal(workspaceVersionFrom(toml), '1.2.3');
});

test('a Cargo.toml without [workspace.package] is an error, not a guess', () => {
  assert.throws(() => workspaceVersionFrom('[package]\nversion = "1"\n'), /workspace\.package/);
});

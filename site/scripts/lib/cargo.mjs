// Reads the release version out of Cargo.toml text. It takes the text rather than a path so that both
// the build scripts and the bundled landing page can use it: the page build runs from a bundle, where
// a path worked out from this file's own location would point inside dist/.

/** The version in [workspace.package]. A line scan, so arrays above `version` do not matter. */
export function workspaceVersionFrom(cargoToml) {
  const lines = cargoToml.split('\n');
  const start = lines.findIndex((line) => line.trim() === '[workspace.package]');
  if (start !== -1) {
    for (let i = start + 1; i < lines.length && !/^\s*\[/.test(lines[i]); i += 1) {
      const match = lines[i].match(/^\s*version\s*=\s*"([^"]+)"/);
      if (match) return match[1];
    }
  }
  throw new Error('Could not read the [workspace.package] version from Cargo.toml');
}

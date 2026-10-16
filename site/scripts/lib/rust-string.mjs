// Reads a Rust string literal out of Rust source text. The CLI help is a string in
// crates/refhir/src, and the site publishes it as the command-line reference, so it has to come
// from the source: the Cloudflare build has no Rust toolchain and no binary to ask.
//
// Handled: the escapes `\n`, `\"` and `\\`, and a backslash before a line break, which Rust treats
// as a continuation that also skips the whitespace on the next line. Anything else throws, so a
// help text that starts using another escape fails loudly instead of rendering wrong.

/**
 * The text of the first string literal after `marker` in `source`.
 *
 * With `format: true` the literal is a `println!` format string, where `{{` and `}}` stand for
 * literal braces.
 */
export function rustStringAfter(source, marker, { format = false } = {}) {
  const at = source.indexOf(marker);
  if (at === -1) throw new Error(`rust-string: "${marker}" not found`);
  const open = source.indexOf('"', at + marker.length);
  if (open === -1) throw new Error(`rust-string: no string literal after "${marker}"`);

  let out = '';
  let i = open + 1;
  for (;;) {
    if (i >= source.length) throw new Error(`rust-string: unterminated string after "${marker}"`);
    const c = source[i];
    if (c === '"') break;
    if (c !== '\\') {
      out += c;
      i += 1;
      continue;
    }
    const next = source[i + 1];
    if (next === 'n') out += '\n';
    else if (next === '"') out += '"';
    else if (next === '\\') out += '\\';
    else if (next === '\n' || next === '\r') {
      i += 2;
      while (i < source.length && /\s/.test(source[i])) i += 1;
      continue;
    } else {
      throw new Error(`rust-string: unsupported escape \\${next} after "${marker}"`);
    }
    i += 2;
  }
  return format ? out.replace(/\{\{/g, '{').replace(/\}\}/g, '}') : out;
}

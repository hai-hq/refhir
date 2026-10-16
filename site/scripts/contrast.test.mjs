// Locks in the text colour pairs the landing page relies on. Every colour is read from the stylesheets:
// tokens from brand.css, and values set in a rule from landing.css. Changing one to something
// unreadable fails here instead of shipping. Ratios are WCAG 2.x
// relative luminance. Body text needs 4.5. Non-text marks (the red dot and rail) need 3.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const styles = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', 'src', 'styles');
const landingCss = fs.readFileSync(path.join(styles, 'landing.css'), 'utf8');
const css = fs.readFileSync(path.join(styles, 'brand.css'), 'utf8') + landingCss;

const token = (name) => {
  const match = css.match(new RegExp(`${name}:\\s*(#[0-9a-fA-F]{6})`));
  assert.ok(match, `token ${name} is not defined as a six-digit hex in the stylesheets`);
  return match[1];
};

// A colour a rule in landing.css sets, for example declared('.tile__note', 'color').
const declared = (selector, property) => {
  const at = landingCss.indexOf(`${selector} {`);
  assert.ok(at !== -1, `landing.css has no rule for ${selector}`);
  const body = landingCss.slice(at, landingCss.indexOf('}', at));
  const match = body.match(new RegExp(`(?:^|[\\s;{])${property}:\\s*(#[0-9a-fA-F]{6}|var\\((--[a-z-]+)\\))`));
  assert.ok(match, `${selector} does not set ${property} to a six-digit hex or a brand token`);
  return match[2] ? token(match[2]) : match[1];
};
const dark = (property) => declared(":root[data-theme='dark']", property);

const channel = (c) => {
  const v = c / 255;
  return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;
};
const luminance = (hex) => {
  const n = parseInt(hex.slice(1), 16);
  return 0.2126 * channel((n >> 16) & 255) + 0.7152 * channel((n >> 8) & 255) + 0.0722 * channel(n & 255);
};
const ratio = (a, b) => {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
};

const WHITE = '#ffffff';
const pairs = [
  // [label, foreground, background, minimum]
  ['body text on white', token('--rf-charcoal'), WHITE, 4.5],
  ['muted text on white', token('--rf-grey'), WHITE, 4.5],
  ['muted text on a panel', token('--rf-grey'), token('--rf-snow'), 4.5],
  ['links on white', token('--rf-link'), WHITE, 4.5],
  ['links on a panel', token('--rf-link'), token('--rf-snow'), 4.5],
  ['white text on the red button', WHITE, token('--rf-red'), 4.5],
  ['white text on the red button, hovered', WHITE, declared('.btn--primary:hover', 'background'), 4.5],
  ['dark text on red-on-dark', token('--rf-night'), token('--rf-red-on-dark'), 4.5],
  ['body text on the dark page', dark('--fg'), token('--rf-night'), 4.5],
  ['muted text on the dark page', dark('--muted'), token('--rf-night'), 4.5],
  ['muted text on a dark panel', dark('--muted'), dark('--panel'), 4.5],
  ['muted text on the darkest band', dark('--muted'), dark('--band'), 4.5],
  ['links on the dark page', token('--rf-link-on-dark'), token('--rf-night'), 4.5],
  ['links on the darkest band', token('--rf-link-on-dark'), dark('--band'), 4.5],
  ['text on a charcoal tile', declared('.tile', 'color'), token('--rf-charcoal'), 4.5],
  ['tile notes on a charcoal tile', declared('.tile__note', 'color'), token('--rf-charcoal'), 4.5],
  ['muted text on the light-mode band', declared('.band', '--muted'), token('--rf-charcoal'), 4.5],
  ['muted text on a light-mode band panel', declared('.band', '--muted'), declared('.band', '--panel'), 4.5],
  ['code comments on the code block', declared(".astro-code span[style*='6A737D']", 'color'), token('--rf-night'), 4.5],
  ['terminal notes on the terminal', declared('.term__note', 'color'), token('--rf-night'), 4.5],
  // Small red text: the section numbers (.kicker::before) and step numbers (.flow li::before).
  ['small red text on white', token('--rf-red'), WHITE, 4.5],
  ['small red-on-dark text on the dark page', token('--rf-red-on-dark'), token('--rf-night'), 4.5],
  ['small red-on-dark text on the darkest band', token('--rf-red-on-dark'), dark('--band'), 4.5],
  ['small red-on-dark text on the light-mode band', token('--rf-red-on-dark'), token('--rf-charcoal'), 4.5],
  ['the live tag on the terminal', declared('.term__live', 'color'), token('--rf-night'), 4.5],
  ['red mark on white (non-text)', token('--rf-red'), WHITE, 3],
  ['red-on-dark mark on the dark page (non-text)', token('--rf-red-on-dark'), token('--rf-night'), 3],
];

for (const [label, fg, bg, minimum] of pairs) {
  test(`${label} is at least ${minimum}:1`, () => {
    const got = ratio(fg, bg);
    assert.ok(got >= minimum, `${fg} on ${bg} is ${got.toFixed(2)}:1, below ${minimum}:1`);
  });
}

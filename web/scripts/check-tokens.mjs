// Checks the colour tokens of app/global.css, offline: the WCAG 2 contrast ratio of every pair
// the site design declares (section 2.2), in both themes (S31).
//
//   text pairs       text, foreground, muted-foreground, primary, verdict-met, destructive on
//                    background, card and muted: at least 4.5
//   control pairs    input, ring, rule-strong on background and card: at least 3
//   action label     primary-foreground on primary: at least 4.5
//
// The light theme is `:root`, the dark theme `.dark` over it; `var(--x)` resolves within the
// theme. A token it cannot find, or a value that is not a hex colour, is a failure.
//
//   node scripts/check-tokens.mjs             over app/global.css
//   node scripts/check-tokens.mjs --selftest  a passing fixture, then one failure of each kind
//
// Exit 0 when every pair passes, 1 otherwise, with one line per failure.
import { existsSync, readFileSync } from 'node:fs';
import * as path from 'node:path';

const CSS = path.resolve(import.meta.dirname, '..', 'app/global.css');

// WCAG 2.2: 1.4.3 for text, 1.4.11 for control edges and focus (S31).
const TEXT_MIN = 4.5;
const CONTROL_MIN = 3;

const TEXT_TOKENS = ['text', 'foreground', 'muted-foreground', 'primary', 'verdict-met', 'destructive'];
const TEXT_GROUNDS = ['background', 'card', 'muted'];
const CONTROL_TOKENS = ['input', 'ring', 'rule-strong'];
const CONTROL_GROUNDS = ['background', 'card'];

/** Every declared pair: foreground, background, minimum ratio. */
const PAIRS = [
  ...TEXT_TOKENS.flatMap((fg) => TEXT_GROUNDS.map((bg) => [fg, bg, TEXT_MIN])),
  ...CONTROL_TOKENS.flatMap((fg) => CONTROL_GROUNDS.map((bg) => [fg, bg, CONTROL_MIN])),
  ['primary-foreground', 'primary', TEXT_MIN],
];

// A top-level rule block: `:root {` or `.dark {` at the start of a line, to its closing brace.
const MAX_VAR_DEPTH = 8;

/** The declarations of the first top-level block with this selector, or null. */
function blockOf(css, selector) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const match = new RegExp(`^${escaped}\\s*\\{([^}]*)\\}`, 'm').exec(css);
  if (!match) {
    return null;
  }
  const tokens = new Map();
  for (const [, name, value] of match[1].matchAll(/--([\w-]+)\s*:\s*([^;]+);/g)) {
    tokens.set(name, value.trim());
  }
  return tokens;
}

/** A token's hex value in a theme, following `var(--x)`; an error string when it cannot. */
function resolve(theme, name) {
  let value = theme.get(name);
  for (let depth = 0; depth < MAX_VAR_DEPTH && value?.startsWith('var('); depth++) {
    value = theme.get(/^var\(--([\w-]+)\)$/.exec(value)?.[1]);
  }
  if (value === undefined) {
    return { error: `--${name} not found` };
  }
  if (!/^#([0-9a-f]{3}|[0-9a-f]{6})$/i.test(value)) {
    return { error: `--${name} is ${value}, not a hex colour` };
  }
  return { hex: value };
}

/** The relative luminance of a hex colour (WCAG 2). */
function luminance(hex) {
  const digits = hex.length === 4 ? [...hex.slice(1)].map((d) => d + d).join('') : hex.slice(1);
  const [r, g, b] = [0, 2, 4].map((i) => parseInt(digits.slice(i, i + 2), 16) / 255);
  const linear = (c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
  return 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
}

/** The contrast ratio of two hex colours, 1 to 21. */
function contrast(a, b) {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

/** Every failing pair of a stylesheet's text, as lines; and the pairs checked. */
function checkCss(css) {
  const failures = [];
  const light = blockOf(css, ':root');
  const dark = blockOf(css, '.dark');
  if (!light || !dark) {
    return { failures: [`no ${light ? '.dark' : ':root'} block`], checked: 0 };
  }

  // The dark theme inherits what it does not redeclare.
  const themes = [['light', light], ['dark', new Map([...light, ...dark])]];
  let checked = 0;
  for (const [theme, tokens] of themes) {
    for (const [fg, bg, min] of PAIRS) {
      const [a, b] = [resolve(tokens, fg), resolve(tokens, bg)];
      const error = a.error ?? b.error;
      if (error) {
        failures.push(`${theme}: ${fg} on ${bg}: ${error}`);
        continue;
      }
      checked += 1;
      const ratio = contrast(a.hex, b.hex);
      if (ratio < min) {
        failures.push(`${theme}: ${fg} ${a.hex} on ${bg} ${b.hex}: ${ratio.toFixed(2)}, at least ${min}`);
      }
    }
  }
  return { failures, checked };
}

// Selftest: the design's tokens pass; a low ratio, a missing token and a non-hex value fail.
const FIXTURE = `:root {
  --background: #fbfbf9;
  --card: #f3f3f0;
  --muted: #ebebe6;
  --foreground: #191b20;
  --text: #2b2e35;
  --muted-foreground: #5c626c;
  --primary: #5b3ee6;
  --primary-foreground: #ffffff;
  --destructive: #b42318;
  --verdict-met: #1a6b3c;
  --input: #7c818b;
  --ring: #b45309;
  --rule-strong: var(--input);
}

.dark {
  --background: #111317;
  --card: #181b21;
  --muted: #20242c;
  --foreground: #eceef2;
  --text: #d1d5dc;
  --muted-foreground: #9da4b0;
  --primary: #9783fe;
  --primary-foreground: #111317;
  --destructive: #f97066;
  --verdict-met: #6fd39a;
  --input: #7a8190;
  --ring: #f5a524;
}
`;

const BREAKS = [
  ['a low ratio', (css) => css.replace('--muted-foreground: #5c626c;', '--muted-foreground: #a0a4ab;')],
  ['a missing token', (css) => css.replace('  --verdict-met: #6fd39a;\n', '').replace('  --verdict-met: #1a6b3c;\n', '')],
  ['a value that is not hex', (css) => css.replace('--ring: #f5a524;', '--ring: oklch(0.8 0.15 75);')],
];

function selftest() {
  const problems = [];
  const clean = checkCss(FIXTURE);
  clean.failures.forEach((f) => problems.push(`clean fixture fails: ${f}`));
  for (const [what, breakIt] of BREAKS) {
    const broken = breakIt(FIXTURE);
    if (broken === FIXTURE) {
      problems.push(`${what}: the fixture edit matched nothing`);
      continue;
    }
    const found = checkCss(broken).failures;
    if (found.length === 0) {
      problems.push(`${what}: not reported`);
      continue;
    }
    console.log(`check-tokens: selftest ${what} finds ${found[0]}`);
  }
  problems.forEach((p) => console.error(`check-tokens: selftest FAIL ${p}`));
  console.log(`check-tokens: selftest ${problems.length === 0 ? 'ok' : `${problems.length} failures`}`);
  process.exit(problems.length === 0 ? 0 : 1);
}

function main() {
  if (!existsSync(CSS)) {
    console.error('check-tokens: FAIL app/global.css missing');
    process.exit(1);
  }
  const { failures, checked } = checkCss(readFileSync(CSS, 'utf8'));
  failures.forEach((f) => console.error(`check-tokens: FAIL ${f}`));
  console.log(`check-tokens: ${checked} of ${PAIRS.length * 2} pairs measured; ${failures.length === 0 ? 'ok' : `${failures.length} failures`}`);
  process.exit(failures.length === 0 ? 0 : 1);
}

if (process.argv.includes('--selftest')) {
  selftest();
} else {
  main();
}

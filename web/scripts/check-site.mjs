// Checks the static export in out/ (run after `npm run build`), offline:
//
//   1. pages     every docs Markdown file and CHANGELOG.md has its page, and so do the product routes
//   2. links     every internal href and src resolves to a file in out/, every #anchor to an id,
//                and no link inside another (a browser re-nests it, and React's hydration fails)
//   3. origins   no script, stylesheet or font from another origin; no Google Fonts request
//   4. csp       every page's meta policy allows each of its inline scripts by hash
//   5. budget    first-load JS (gzip) of the product pages within budget; every page reported
//   6. files     install.sh as scripts/install.sh, _headers with the noindex host rules,
//                robots.txt, sitemap.xml, llms.txt
//
// Exit 0 when every check passes, 1 otherwise, with one line per failure.
import { createHash } from 'node:crypto';
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import * as path from 'node:path';
import { gzipSync } from 'node:zlib';

const WEB = path.resolve(import.meta.dirname, '..');
const REPO = path.resolve(WEB, '..');
const OUT = path.join(WEB, 'out');

// Product pages: a ceiling on first-load JS, gzip. Measured 139.3 kB on /, /product and
// /roadmap at the first build; an empty page of the pinned Next and React is 132.0 kB (README).
const PRODUCT_JS_BUDGET_KB = 140;
const PRODUCT_PAGES = /^(index|product(\/.*)?|roadmap)\.html$/;

// The same exclusions as lib/docs-paths.ts, written again here so the check does not trust it.
const EXCLUDED_DOCS = ['superpowers/', 'reference/notes/', 'examples/'];
const OWN_POLICY_DIRS = ['explainers/'];

const REQUIRED_FILES = ['robots.txt', 'sitemap.xml', 'llms.txt', '_headers', 'install.sh', '404.html'];
const NOINDEX_HOST = 'https://get.swarmops.cc/*';

/** The base64 sha256 of a script's text, as CSP hashes it. */
function sha256(text) {
  return createHash('sha256')
    .update(text, 'utf8')
    .digest('base64');
}

const failures = [];
const fail = (message) => failures.push(message);

/** Every file under `dir`, relative to it, with `/` separators. */
function walk(dir, prefix = '') {
  if (!existsSync(dir)) {
    return [];
  }
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const rel = prefix + entry.name;
    return entry.isDirectory() ? walk(path.join(dir, entry.name), `${rel}/`) : [rel];
  });
}

/** The out/ file a site path is served from, or null. `/docs/perf` -> docs/perf.html. */
function served(sitePath) {
  const clean = decodeURIComponent(sitePath)
    .replace(/^\/+/, '')
    .replace(/\/+$/, '');
  const candidates = clean === '' ? ['index.html'] : [clean, `${clean}.html`, `${clean}/index.html`];
  return candidates.find((c) => existsSync(path.join(OUT, c)) && statSync(path.join(OUT, c)).isFile()) ?? null;
}

const ids = new Map();
/** The element ids of an HTML file. */
function idsOf(file) {
  if (!ids.has(file)) {
    const html = readFileSync(path.join(OUT, file), 'utf8');
    ids.set(file, new Set([...html.matchAll(/\sid="([^"]+)"/g)].map((m) => m[1])));
  }
  return ids.get(file);
}

// 1. Pages: one per docs Markdown file, as lib/docs-paths.ts maps them.
function checkPages(htmlFiles) {
  const docs = walk(path.join(REPO, 'docs'))
    .filter((f) => f.endsWith('.md') && !EXCLUDED_DOCS.some((d) => f.startsWith(d)));
  const expected = docs.map((f) => {
    const slug = f
      .slice(0, -'.md'.length)
      .replace(/(^|\/)README$/, '');
    return slug === '' ? 'docs.html' : `docs/${slug}.html`;
  });
  expected.push('docs.html', 'docs/changelog.html', 'index.html', 'product.html', 'roadmap.html');
  for (const page of new Set(expected)) {
    if (!htmlFiles.includes(page)) {
      fail(`pages: ${page} missing`);
    }
  }
  return docs.length;
}

// 2. Links: internal hrefs and srcs resolve, anchors exist.
function checkLinks(file, html) {
  const refs = [...html.matchAll(/\s(?:href|src)="([^"]*)"/g)].map((m) => m[1].replaceAll('&amp;', '&'));
  for (const ref of refs) {
    if (/^[a-z][a-z0-9+.-]*:|^\/\//i.test(ref)) {
      continue;
    }
    if (!ref.startsWith('/') && !ref.startsWith('#')) {
      fail(`links: ${file}: relative link ${ref}`);
      continue;
    }
    const [target, hash] = ref.split('#');
    const resolved = target === '' ? file : served(target.split('?')[0]);
    if (resolved === null) {
      fail(`links: ${file}: ${ref} resolves to no file`);
      continue;
    }
    if (hash && resolved.endsWith('.html') && !idsOf(resolved).has(decodeURIComponent(hash))) {
      fail(`links: ${file}: ${ref}: no id "${hash}" in ${resolved}`);
    }
  }
  return refs.length;
}

// 2b. No <a> inside another <a>.
function checkNesting(file, html) {
  let depth = 0;
  for (const [tag] of html.matchAll(/<\/?a[\s>]/g)) {
    depth += tag.startsWith('</') ? -1 : 1;
    if (depth > 1) {
      fail(`links: ${file}: a link inside a link`);
      return;
    }
  }
}

// 3. Origins: nothing executable, styled or typeset from elsewhere.
function checkOrigins(file, text) {
  const external = [
    ...text.matchAll(/<script[^>]*\ssrc="(https?:)?\/\/[^"]*"/g),
    ...text.matchAll(/<link[^>]*rel="(?:stylesheet|preload|modulepreload)"[^>]*href="(?:https?:)?\/\/[^"]*"/g),
    ...text.matchAll(/url\((["']?)(https?:)?\/\/[^)]*\)/g),
    ...text.matchAll(/@import\s+(url\()?["']?(https?:)?\/\//g),
    ...text.matchAll(/fonts\.(googleapis|gstatic)\.com/g),
  ];
  for (const match of external) {
    fail(`origins: ${file}: ${match[0].slice(0, 120)}`);
  }
}

// 4. CSP: the meta policy names every inline script's hash.
function checkCsp(file, html) {
  const policy = /<meta http-equiv="Content-Security-Policy" content="([^"]*)"/.exec(html);
  if (!policy) {
    fail(`csp: ${file}: no meta policy`);
    return;
  }
  if (policy.index > html.indexOf('<script')) {
    fail(`csp: ${file}: the meta policy comes after a script`);
  }
  for (const [, body] of html.matchAll(/<script(?![^>]*\ssrc=)[^>]*>([\s\S]*?)<\/script>/g)) {
    const hash = `'sha256-${sha256(body)}'`;
    if (!policy[1].includes(hash)) {
      fail(`csp: ${file}: inline script ${hash} not allowed`);
    }
  }
}

// 5. Budget: the scripts a page loads before any interaction, gzip-compressed. `noModule`
// scripts (polyfills) load only in browsers without ES modules, so they do not count.
const gzipped = new Map();
function firstLoadKb(html) {
  const scripts = [...html.matchAll(/<script([^>]*)\ssrc="(\/[^"]+)"([^>]*)>/g)]
    .filter((m) => !/\snoModule/i.test(m[1] + m[3]))
    .map((m) => m[2]);
  let bytes = 0;
  for (const src of new Set(scripts)) {
    if (!gzipped.has(src)) {
      gzipped.set(src, gzipSync(readFileSync(path.join(OUT, src))).length);
    }
    bytes += gzipped.get(src);
  }
  return bytes / 1024;
}

// 6. Files the site serves beside its pages.
function checkFiles() {
  for (const name of REQUIRED_FILES) {
    if (!existsSync(path.join(OUT, name))) {
      fail(`files: ${name} missing`);
    }
  }
  const install = path.join(OUT, 'install.sh');
  if (existsSync(install) && !readFileSync(install).equals(readFileSync(path.join(REPO, 'scripts/install.sh')))) {
    fail('files: install.sh differs from scripts/install.sh');
  }
  const headers = path.join(OUT, '_headers');
  if (existsSync(headers) && !readFileSync(headers, 'utf8').includes(`${NOINDEX_HOST}\n  X-Robots-Tag: noindex`)) {
    fail(`files: _headers lacks the noindex rule for ${NOINDEX_HOST}`);
  }
}

function main() {
  if (!existsSync(OUT)) {
    console.error('check-site: no out/; run `npm run build` first');
    process.exit(1);
  }

  const files = walk(OUT);
  const html = files.filter((f) => f.endsWith('.html'));
  const docCount = checkPages(html);

  // Per page: links, origins, policy, budget.
  let links = 0;
  const budgets = [];
  for (const file of html) {
    const text = readFileSync(path.join(OUT, file), 'utf8');
    links += checkLinks(file, text);
    checkNesting(file, text);
    checkOrigins(file, text);
    if (OWN_POLICY_DIRS.some((dir) => file.startsWith(dir))) {
      continue;
    }
    checkCsp(file, text);
    const kb = firstLoadKb(text);
    budgets.push([file, kb]);
    if (PRODUCT_PAGES.test(file) && kb > PRODUCT_JS_BUDGET_KB) {
      fail(`budget: ${file}: ${kb.toFixed(1)} kB first-load JS (gzip), budget ${PRODUCT_JS_BUDGET_KB} kB`);
    }
  }

  // Stylesheets and scripts may not pull from other origins either.
  for (const file of files.filter((f) => f.endsWith('.css'))) {
    checkOrigins(file, readFileSync(path.join(OUT, file), 'utf8'));
  }
  checkFiles();

  // Report.
  const largest = budgets.toSorted((a, b) => b[1] - a[1]);
  const product = budgets
    .filter(([f]) => PRODUCT_PAGES.test(f))
    .map(([, kb]) => kb);
  console.log(`check-site: ${html.length} HTML pages (${docCount} docs Markdown files), ${links} links checked`);
  console.log(
    `check-site: first-load JS (gzip): product pages max ${Math.max(...product).toFixed(1)} kB ` +
      `(budget ${PRODUCT_JS_BUDGET_KB} kB); largest page ${largest[0][0]} ${largest[0][1].toFixed(1)} kB`,
  );
  for (const message of failures) {
    console.error(`check-site: FAIL ${message}`);
  }
  console.log(`check-site: ${failures.length === 0 ? 'ok' : `${failures.length} failures`}`);
  process.exit(failures.length === 0 ? 0 : 1);
}

main();

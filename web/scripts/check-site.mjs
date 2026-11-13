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
// Then the rules of the site design (section 1), one function each, named as the design names
// them; the requirement ids are in brackets:
//
//   structure          skip link first, header next, one h1, no skipped level, landmarks [S15, S39]
//   header-links       at most 6 text links in the header nav, each address once; the
//                      repository linked by name in header and footer [S22, S25]
//   footer-map         the footer's 16 destinations on every page [S28]
//   release-links      a link named by a version, Changelog or Releases goes to /docs/changelog [S23]
//   home-first-screen  Home: h1 <= 10 words, next sentence <= 30; h1 < install line < action [S1]
//   home-primary       Home: two data-primary controls, one address, hero and last section [S2]
//   home-order         Home: ids sample < features < evidence [S12]
//   feature-words      Home: each feature <= 45 words with exactly one guide link [S13]
//   home-words         Home: <= 1,200 words in main outside pre [S14]
//   copy-control       Home: button[data-copy] with a name, a live region, the line as text [S6]
//   platforms          Home's architectures equal the target triples of out/install.sh [S8]
//   mcp-command        Home's MCP registration line occurs byte for byte in the MCP guide [S11]
//   next-version       product pages: a version in a "next" sentence is above the latest [S17]
//   tool-comparison    product pages: a comparison with git says it is from the design [S20]
//   comparative-link   product pages: faster, slower, smaller or × links the ledger [S4]
//   praise-words       product pages: no word of the praise list [S4]
//   blockquote-source  product pages: every blockquote carries a link [S21]
//   image-text         every img has a text alternative, every svg image a name [S35]
//   names              every link and button has text or aria-label [S40]
//   motion             own transitions <= 200 ms, no own animation, reduced-motion rule [S34]
//   literal-size       no text-[...] literal in app/ or components/ [S32]
//   literal-space      no [..px] spacing literal and no half step there [S33]
//   llms               llms.txt in its format, every twin once, the agents page first [S16, D44]
//   twin-links         every docs page names its Markdown twin; twin links resolve [D45, D49]
//   lookup             /api/lookup holds every command, tool and E_* code, each resolves [S24, D32]
//   addresses          every line of addresses.txt is a page or a redirect source [D18]
//   readme-home-drift  README's install line, version and status word equal Home's [S48]
//   home-transfer      Home's gzip transfer reported; above 300 kB fails [S38]
//
// A rule in ENFORCED fails the run; any other rule's findings print as PENDING, for a fix
// slice to clear before the rule joins ENFORCED. An empty input (no page, no table) fails.
//
//   node scripts/check-site.mjs                    every check over out/
//   node scripts/check-site.mjs --selftest         each rule finds a planted violation
//   node scripts/check-site.mjs --pending          as the first, with every PENDING line
//   node scripts/check-site.mjs --write-addresses  rewrites addresses.txt from out/
//
// Exit 0 when every check passes, 1 otherwise, with one line per failure.
import { createHash, randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import * as path from 'node:path';
import { gzipSync } from 'node:zlib';

const WEB = path.resolve(import.meta.dirname, '..');
const REPO = path.resolve(WEB, '..');
const OUT = path.join(WEB, 'out');

// Product pages: a ceiling on first-load JS, gzip. Measured 139.3 kB on /, /product and
// /roadmap at the first build; an empty page of the pinned Next and React is 132.0 kB (README).
// /now joined with 137.2 kB.
const PRODUCT_JS_BUDGET_KB = 140;
const PRODUCT_PAGES = /^(index|product(\/.*)?|roadmap|now)\.html$/;

// The same exclusions as lib/docs-paths.ts, written again here so the check does not trust it.
const EXCLUDED_DOCS = ['superpowers/', 'reference/notes/', 'examples/'];
const OWN_POLICY_DIRS = ['explainers/'];

const REQUIRED_FILES = ['robots.txt', 'sitemap.xml', 'llms.txt', '_headers', 'install.sh', '404.html', '_redirects', '.well-known/security.txt'];
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

// ---------------------------------------------------------------------------------------------
// The site design's rules (section 1). Each takes a site root (`siteAt`) and returns its
// findings as `file:line: message`, so the selftest runs them over a fixture tree.

const SITE_URL = 'https://swarmops.cc';
const REPO_URL = 'https://github.com/KeiaiLab/SwarmOps';
const REPO_FILE = `${REPO_URL}/blob/stable/`;
const HOME = 'index.html';
const VS_GIT = 'product/vs-git.html';
const INSTALL_LINE = 'curl -fsSL https://get.swarmops.cc/install.sh | bash'; // S6, S48

// Limits.
const HEADER_LINKS_MAX = 6; // S22
const HOME_H1_WORDS = 10; // S1
const HOME_LEAD_WORDS = 30; // S1
const HOME_PRIMARY_COUNT = 2; // S2
const HOME_ANCHORS = ['sample', 'features', 'evidence']; // S12
const FEATURE_WORDS = 45; // S13
const FEATURE_GUIDE_LINKS = 1; // S13
const HOME_WORDS = 1200; // S14
const MOTION_MAX_MS = 200; // S34
const HOME_TRANSFER_KB = 300; // S38
const PENDING_SHOWN = 20;

// The footer's destinations, written again here so the check does not trust lib/site.ts (S28).
const FOOTER_MAP = [
  '/docs/guides/quick-start',
  '/docs',
  '/docs/reference/cli',
  '/docs/roadmap',
  '/docs/changelog',
  '/docs/goals',
  '/docs/perf',
  '/docs/known-limitations',
  `${REPO_URL}/issues/new/choose`,
  `${REPO_FILE}CONTRIBUTING.md`,
  `${REPO_FILE}CODE_OF_CONDUCT.md`,
  `${REPO_FILE}LICENSE`,
  `${REPO_FILE}SECURITY.md`,
  '/docs/policies/versioning-and-stability',
  '/docs/policies/clean-room',
  REPO_URL,
];

// Words and patterns.
const RELEASE_TEXT = /^(v?\d+\.\d+\.\d+|changelog|releases)$/i; // S23
const RELEASE_PAGE = '/docs/changelog'; // S23
// S4: the maintained praise list.
const PRAISE_LIST = [
  'fastest', 'blazing', 'blazingly', 'lightning', 'best', 'best-in-class', 'world-class', 'revolutionary',
  'seamless', 'seamlessly', 'effortless', 'effortlessly', 'unparalleled', 'unmatched', 'cutting-edge',
  'state-of-the-art',
];
const PRAISE_WORDS = new RegExp(`\\b(${PRAISE_LIST.join('|')})\\b`, 'gi');
const COMPARATIVE = /\b(faster|slower|smaller)\b|×/i; // S4
const LEDGER_LINK = /\shref="\/docs\/(perf|goals)(#[^"]*)?"/; // S4
const TOOL_NAME = /\bgit\b/i; // S20
const TOOL_COMPARE = /\b(faster|slower|smaller|larger|fewer|cheaper|quicker)\b|×/i; // S20
const DESIGN_ONLY = /from the design/i; // S20
const NO_BENCHMARK = /no benchmark/i; // S20
const ARCH_WORDS = /\b(x86_64|aarch64|arm64|amd64|armv7|i686|riscv64|ppc64le|s390x)\b/g; // S8
const TRIPLE = /\b([a-z0-9_]+)-unknown-linux-(?:gnu|musl)\b/g; // S8
const UNSUPPORTED = ['macOS', 'Windows']; // S8
const MCP_SERVER = ' -- swarmops mcp'; // S11
const NEXT_WORD = /\bnext\b/i; // S17
const VERSION = /\bv?(\d+)\.(\d+)\.(\d+)\b/g; // S17
const RELEASED_HEADING = /^## \[(\d+)\.(\d+)\.(\d+)\] - \d{4}-\d{2}-\d{2}\s*$/m; // S17
const STATUS_WORD = /\b(pre-release|alpha|beta|experimental)\b/i; // S48
const README_VERSION = /\bv(\d+\.\d+\.\d+)\b/; // S48
const LITERAL_SIZE = 'text-['; // S32
// S33: a spacing utility with a pixel literal or a half step. `mt-[3px]`, `gap-2.5`, `-mb-0.5`.
const SPACING = 'p|px|py|pt|pr|pb|pl|ps|pe|m|mx|my|mt|mr|mb|ml|ms|me|gap|gap-x|gap-y|space-x|space-y';
const LITERAL_SPACE = new RegExp(`(?<![\\w-])-?(?:${SPACING})-(?:\\[[^\\]\\s]*px\\]|\\d+\\.5)(?![\\w.])`, 'g');
const OWN_DURATION = /(?<![\w-])duration-(\d+|\[[^\]\s]+\])/g; // S34
const OWN_ANIMATE = /(?<![\w-])animate-[\w-]+/g; // S34
const REDUCED_MOTION = /prefers-reduced-motion:\s*reduce\)\s*\{\s*\*[^}]*transition-duration:\s*0?\.01ms/; // S34
const SOURCE_DIRS = ['web/app', 'web/components']; // S32, S33, S34
const SOURCE_EXT = /\.(tsx?|css|mdx?)$/;
const AGENTS_TWIN = '/docs/agents.md'; // D47
const LOOKUP = 'api/lookup'; // S24, D32
const ADDRESSES = 'web/addresses.txt'; // D18
const NOT_PAGES = ['404.html', '_not-found.html'];

// Tags a reader can reach with Tab, and tags that are content (none may sit above the header).
const FOCUSABLE = /<(a\s[^>]*href=|button\b|input\b|select\b|textarea\b)[^>]*>/gi;
const CONTENT_TAG = /<(a|button|img|svg|p|h[1-6]|section|aside|nav|input|ul|ol|table|pre)\b/i;
const VOID_TAGS = new Set(['img', 'br', 'hr', 'input', 'meta', 'link', 'source', 'wbr', 'area', 'col', 'embed']);
const ENTITIES = { amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: ' ' };

/** A site root: the repository, web/ and the export in web/out/. */
function siteAt(repo) {
  return { repo, web: path.join(repo, 'web'), out: path.join(repo, 'web', 'out'), cache: new Map(), notes: [] };
}

/** A file under the export, read once. */
function outText(site, rel) {
  if (!site.cache.has(rel)) {
    site.cache.set(rel, readFileSync(path.join(site.out, rel), 'utf8'));
  }
  return site.cache.get(rel);
}

/** The HTML pages of the export, without the explainers (their own frame). */
function pagesOf(site) {
  return walk(site.out)
    .filter((f) => f.endsWith('.html') && !OWN_POLICY_DIRS.some((d) => f.startsWith(d)));
}

/** Product pages: Home, /product/**, /roadmap and /now. */
function productPages(site) {
  return pagesOf(site).filter((f) => PRODUCT_PAGES.test(f));
}

/** Docs pages: /docs and everything under it. */
function docsPages(site) {
  return pagesOf(site).filter((f) => f === 'docs.html' || f.startsWith('docs/'));
}

/** The 1-based line of an offset. */
function lineAt(text, index) {
  let line = 1;
  for (let i = text.indexOf('\n'); i !== -1 && i < index; i = text.indexOf('\n', i + 1)) {
    line += 1;
  }
  return line;
}

/** Scripts, styles, templates and comments blanked out, offsets kept. */
function blank(html) {
  const spaces = (m) => m.replace(/[^\n]/g, ' ');
  return html
    .replace(/<(script|style|template)\b[^>]*>[\s\S]*?<\/\1>/gi, spaces)
    .replace(/<!--[\s\S]*?-->/g, spaces);
}

/** HTML entities as text. `&amp;` -> `&`. */
function decode(text) {
  return text.replace(/&(#x[0-9a-f]+|#\d+|[a-z]+);/gi, (m, e) => {
    if (e[0] !== '#') {
      return ENTITIES[e.toLowerCase()] ?? m;
    }
    const code = e[1].toLowerCase() === 'x' ? parseInt(e.slice(2), 16) : parseInt(e.slice(1), 10);
    return String.fromCodePoint(code);
  });
}

/** The visible text of an HTML fragment, spaces collapsed. */
function textOf(html) {
  const noTags = blank(html).replace(/<[^>]+>/g, ' ');
  return decode(noTags)
    .replace(/\s+/g, ' ')
    .trim();
}

/** Words: runs with a letter or digit. */
function wordCount(text) {
  return text.split(/\s+/).filter((w) => /[\p{L}\p{N}]/u.test(w)).length;
}

/** An attribute's value, decoded, or null. */
function attrOf(tag, name) {
  const match = new RegExp(`\\s${name}="([^"]*)"`).exec(tag);
  return match ? decode(match[1]) : null;
}

/** Whether a tag carries an attribute, with or without a value. */
function hasAttr(tag, name) {
  return new RegExp(`\\s${name}(=|[\\s>/])`).test(tag);
}

/** The element whose opening tag starts at `start`: its tag, inner HTML and end offset. */
function elementAt(html, start) {
  const open = /^<([a-z][a-z0-9-]*)\b[^>]*>/i.exec(html.slice(start, start + 4096));
  if (!open) {
    return null;
  }
  const name = open[1].toLowerCase();
  const innerStart = start + open[0].length;
  if (VOID_TAGS.has(name) || open[0].endsWith('/>')) {
    return { tag: open[0], inner: '', start, end: innerStart };
  }

  // Count nested tags of the same name until the matching close.
  const scan = new RegExp(`<(/?)${name}\\b[^>]*>`, 'gi');
  scan.lastIndex = innerStart;
  let depth = 1;
  for (let m = scan.exec(html); m; m = scan.exec(html)) {
    depth += m[1] ? -1 : 1;
    if (depth === 0) {
      return { tag: open[0], inner: html.slice(innerStart, m.index), start, end: m.index + m[0].length };
    }
  }
  return { tag: open[0], inner: html.slice(innerStart), start, end: html.length };
}

/** Every element of one tag name, outermost first. */
function elementsOf(html, name) {
  const found = [];
  const open = new RegExp(`<${name}\\b`, 'gi');
  for (let m = open.exec(html); m; m = open.exec(html)) {
    const element = elementAt(html, m.index);
    if (element) {
      found.push(element);
    }
  }
  return found;
}

/** The element with an id, or null. */
function byId(html, id) {
  const match = new RegExp(`<[a-z][a-z0-9-]*\\b[^>]*\\sid="${id}"`, 'i').exec(html);
  return match ? elementAt(html, match.index) : null;
}

/** The ids of a page of the export. */
function pageIds(site, file) {
  return new Set([...outText(site, file).matchAll(/\sid="([^"]+)"/g)].map((m) => decode(m[1])));
}

/** The export file a site path is served from, or null. */
function servedIn(site, sitePath) {
  const clean = decodeURIComponent(sitePath)
    .replace(/^\/+/, '')
    .replace(/\/+$/, '');
  const candidates = clean === '' ? ['index.html'] : [clean, `${clean}.html`, `${clean}/index.html`];
  return candidates.find((c) => existsSync(path.join(site.out, c)) && statSync(path.join(site.out, c)).isFile()) ?? null;
}

/** Whether a site address (path and anchor) resolves in the export. */
function resolves(site, address) {
  const [target, hash] = address.split('#');
  const file = servedIn(site, target.split('?')[0]);
  if (file === null) {
    return false;
  }
  return !hash || !file.endsWith('.html') || pageIds(site, file).has(decodeURIComponent(hash));
}

/** Links of a fragment: offset, href, accessible text. */
function linksOf(html) {
  return elementsOf(html, 'a').map((a) => ({
    at: a.start,
    href: attrOf(a.tag, 'href'),
    text: textOf(a.inner),
    name: nameOf(a),
  }));
}

/** The accessible name of a link or button: aria-label, else its text and image alts. */
function nameOf(element) {
  const label = attrOf(element.tag, 'aria-label') ?? attrOf(element.tag, 'aria-labelledby');
  if (label && label.trim()) {
    return label.trim();
  }
  const alts = [...element.inner.matchAll(/<img\b[^>]*\salt="([^"]*)"/g)].map((m) => m[1]).join(' ');
  return `${textOf(element.inner)} ${alts}`.trim() || (attrOf(element.tag, 'title') ?? '').trim();
}

/** Text blocks of a fragment: paragraphs and list items, with their offset. */
function blocksOf(html) {
  return [...elementsOf(html, 'p'), ...elementsOf(html, 'li')];
}

/** The `main` element of a page, or the whole page when it has none. */
function mainOf(html) {
  const at = html.search(/<main\b/i);
  return at === -1 ? { inner: html, start: 0 } : elementAt(html, at);
}

/** A page's blanked HTML: scripts and styles out, offsets kept. */
function pageHtml(site, file) {
  const key = `blank:${file}`;
  if (!site.cache.has(key)) {
    site.cache.set(key, blank(outText(site, file)));
  }
  return site.cache.get(key);
}

/** A finding: `file:line: message`. */
function at(file, html, index, message) {
  return `${file}:${lineAt(html, index)}: ${message}`;
}

/** Home's blanked HTML, or null with a finding when Home is missing. */
function homeOf(site, found) {
  if (!existsSync(path.join(site.out, HOME))) {
    found.push(`${HOME}:0: Home missing`);
    return null;
  }
  return pageHtml(site, HOME);
}

/** A list of pages, or a finding when it is empty: no input is a failure. */
function needPages(pages, what, found) {
  if (pages.length === 0) {
    found.push(`out:0: no ${what}`);
  }
  return pages;
}

// S15, S39: skip link first, the header next, one h1, no skipped level, the four landmarks.
function structure(site) {
  const found = [];
  for (const file of needPages(pagesOf(site), 'pages', found)) {
    const html = pageHtml(site, file);
    const body = Math.max(html.search(/<body\b/i), 0);
    found.push(...skipFirst(file, html, body));
    found.push(...headings(file, html));
    found.push(...landmarks(file, html));
  }
  return found;
}

/** The first focusable element is a link to `main`; nothing but wrappers sits before the header. */
function skipFirst(file, html, body) {
  FOCUSABLE.lastIndex = body;
  const first = FOCUSABLE.exec(html);
  if (!first) {
    return [at(file, html, body, 'no focusable element')];
  }
  const target = (attrOf(first[0], 'href') ?? '').replace(/^#/, '');
  const main = /<main\b[^>]*\sid="([^"]*)"/i.exec(html);
  if (!first[0].startsWith('<a') || !target || !main || main[1] !== target) {
    return [at(file, html, first.index, 'the first focusable element is not a skip link to main')];
  }

  // Between the skip link and the header: no text and no content element.
  const skip = elementAt(html, first.index);
  const header = html.indexOf('<header', skip.end);
  if (header === -1) {
    return [at(file, html, skip.end, 'no header after the skip link')];
  }
  const between = html.slice(skip.end, header);
  if (textOf(between) !== '' || CONTENT_TAG.test(between)) {
    return [at(file, html, skip.end, 'content between the skip link and the header')];
  }
  return [];
}

/** One h1, and each heading at most one level below the one before. */
function headings(file, html) {
  const found = [];
  const levels = [...html.matchAll(/<h([1-6])\b/gi)];
  if (levels.filter((m) => m[1] === '1').length !== 1) {
    found.push(at(file, html, levels[0]?.index ?? 0, `${levels.filter((m) => m[1] === '1').length} h1 elements, not 1`));
  }
  let previous = 0;
  for (const m of levels) {
    const level = Number(m[1]);
    if (level > previous + 1) {
      found.push(at(file, html, m.index, `h${level} after h${previous}: a level skipped`));
    }
    previous = level;
  }
  return found;
}

/** header, nav, one main, one footer; one nav per name. */
function landmarks(file, html) {
  const found = [];
  const count = (name) => (html.match(new RegExp(`<${name}\\b`, 'gi')) ?? []).length;
  for (const name of ['header', 'nav']) {
    if (count(name) === 0) {
      found.push(`${file}:1: no ${name} landmark`);
    }
  }
  for (const name of ['main', 'footer']) {
    if (count(name) !== 1) {
      found.push(`${file}:1: ${count(name)} ${name} elements, not 1`);
    }
  }
  const names = [...html.matchAll(/<nav\b[^>]*>/gi)].map((m) => attrOf(m[0], 'aria-label') ?? '');
  for (const name of new Set(names.filter((n, i) => names.indexOf(n) !== i))) {
    found.push(`${file}:1: two nav elements named "${name}"`);
  }
  return found;
}

// S22, S25: at most 6 text links in the header's nav, each address once; the repository linked
// by a text name in the header and in the footer.
function headerLinks(site) {
  const found = [];
  for (const file of needPages(pagesOf(site), 'pages', found)) {
    const html = pageHtml(site, file);
    const header = elementsOf(html, 'header')[0];
    if (!header) {
      found.push(`${file}:1: no header`);
      continue;
    }

    // 1. The nav's text links.
    const nav = elementsOf(header.inner, 'nav').flatMap((n) => linksOf(n.inner));
    const texts = nav.filter((l) => l.text !== '');
    if (texts.length > HEADER_LINKS_MAX) {
      found.push(at(file, html, header.start, `${texts.length} text links in the header nav, at most ${HEADER_LINKS_MAX}`));
    }
    const hrefs = nav.map((l) => l.href);
    for (const href of new Set(hrefs.filter((h, i) => hrefs.indexOf(h) !== i))) {
      found.push(at(file, html, header.start, `header nav links ${href} twice`));
    }

    // 2. The repository, by name, in both.
    const footer = elementsOf(html, 'footer').at(-1);
    for (const [where, part] of [['header', header], ['footer', footer]]) {
      if (!part || !linksOf(part.inner).some((l) => l.href === REPO_URL && l.name !== '')) {
        found.push(at(file, html, part?.start ?? 0, `${where} has no named link to the repository`));
      }
    }
  }
  return found;
}

// S28: every page's footer links the 16 destinations.
function footerMap(site) {
  const found = [];
  for (const file of needPages(pagesOf(site), 'pages', found)) {
    const html = pageHtml(site, file);
    const footer = elementsOf(html, 'footer').at(-1);
    const hrefs = new Set(footer ? linksOf(footer.inner).filter((l) => l.text).map((l) => l.href) : []);
    const missing = FOOTER_MAP.filter((d) => !hrefs.has(d));
    if (missing.length > 0) {
      found.push(at(file, html, footer?.start ?? 0, `footer lacks ${missing.length} of ${FOOTER_MAP.length}: ${missing.join(', ')}`));
    }
  }
  return found;
}

// S23: a link named by a version, "Changelog" or "Releases" leads into /docs/changelog: every
// link of a product page, and the header and footer of every page (the changelog's own release
// headings link the host's compare views, declined in the design).
function releaseLinks(site) {
  const found = [];
  for (const file of needPages(pagesOf(site), 'pages', found)) {
    const html = pageHtml(site, file);
    const frame = [...elementsOf(html, 'header'), ...elementsOf(html, 'footer')];
    const parts = PRODUCT_PAGES.test(file) ? [{ inner: html, start: 0 }] : frame;
    for (const part of parts) {
      for (const link of linksOf(part.inner)) {
        if (RELEASE_TEXT.test(link.text) && !(link.href ?? '').startsWith(RELEASE_PAGE)) {
          found.push(at(file, html, part.start + link.at, `"${link.text}" links ${link.href}, not ${RELEASE_PAGE}`));
        }
      }
    }
  }
  return found;
}

// S1: Home's h1 at most 10 words, the paragraph after it at most 30; h1 < install line < action.
function homeFirstScreen(site) {
  const found = [];
  const html = homeOf(site, found);
  if (html === null) {
    return found;
  }
  const h1 = elementsOf(html, 'h1')[0];
  if (!h1) {
    return [`${HOME}:1: no h1`];
  }
  const h1Words = wordCount(textOf(h1.inner));
  if (h1Words > HOME_H1_WORDS) {
    found.push(at(HOME, html, h1.start, `h1 has ${h1Words} words, at most ${HOME_H1_WORDS}`));
  }
  const lead = elementsOf(html.slice(h1.end), 'p')[0];
  const leadWords = lead ? wordCount(textOf(lead.inner)) : 0;
  if (!lead || leadWords > HOME_LEAD_WORDS) {
    found.push(at(HOME, html, h1.end, `the sentence after h1 has ${leadWords} words, 1 to ${HOME_LEAD_WORDS}`));
  }

  // DOM order: h1, then the install line as text, then the primary action.
  const install = html.search(installText());
  const primary = html.search(/<[a-z]+\b[^>]*\sdata-primary[\s=>]/i);
  if (!(h1.start < install && install < primary)) {
    found.push(at(HOME, html, h1.start, `order h1 ${h1.start} < install line ${install} < primary ${primary} broken`));
  }
  return found;
}

/** The install line as a text node: between a tag's end and the next tag. */
function installText() {
  const escaped = INSTALL_LINE.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`>\\s*${escaped}\\s*<`);
}

// S2: two data-primary controls with one address, one in the hero, one in the last section.
function homePrimary(site) {
  const found = [];
  const html = homeOf(site, found);
  if (html === null) {
    return found;
  }
  const tags = [...html.matchAll(/<[a-z]+\b[^>]*\sdata-primary[\s=>][^>]*>/gi)];
  if (tags.length !== HOME_PRIMARY_COUNT) {
    return [`${HOME}:1: ${tags.length} data-primary controls, not ${HOME_PRIMARY_COUNT}`];
  }
  const [first, last] = tags.map((m) => ({ at: m.index, href: attrOf(m[0], 'href') }));
  if (first.href !== last.href || !first.href) {
    found.push(at(HOME, html, last.at, `data-primary addresses differ: ${first.href} and ${last.href}`));
  }
  const firstSection = html.search(/<section\b/i);
  const lastSection = html.lastIndexOf('<section');
  if (firstSection !== -1 && first.at > firstSection) {
    found.push(at(HOME, html, first.at, 'the first data-primary is not in the hero'));
  }
  if (lastSection === -1 || last.at < lastSection) {
    found.push(at(HOME, html, last.at, 'the second data-primary is not in the last section'));
  }
  return found;
}

// S12: Home's ids sample, features and evidence, in that order.
function homeOrder(site) {
  const found = [];
  const html = homeOf(site, found);
  if (html === null) {
    return found;
  }
  const offsets = HOME_ANCHORS.map((id) => html.search(new RegExp(`\\sid="${id}"`)));
  const sorted = offsets.every((o, i) => o !== -1 && (i === 0 || o > offsets[i - 1]));
  if (!sorted) {
    found.push(`${HOME}:1: ids ${HOME_ANCHORS.join(' < ')} at ${offsets.join(', ')}: missing or out of order`);
  }
  return found;
}

// S13: each feature (an h3 and what follows it in #features) at most 45 words outside pre, with
// exactly one guide link.
function featureWords(site) {
  const found = [];
  const html = homeOf(site, found);
  if (html === null) {
    return found;
  }
  const section = byId(html, 'features');
  const starts = section ? [...section.inner.matchAll(/<h3\b/gi)].map((m) => m.index) : [];
  if (starts.length === 0) {
    return [`${HOME}:1: no feature (h3) in #features`];
  }
  starts.forEach((start, i) => {
    const part = section.inner.slice(start, starts[i + 1] ?? section.inner.length);
    const words = wordCount(textOf(part.replace(/<pre\b[\s\S]*?<\/pre>/gi, ' ')));
    const guides = linksOf(part).filter((l) => (l.href ?? '').startsWith('/docs/')).length;
    const where = section.start + section.tag.length + start;
    if (words > FEATURE_WORDS) {
      found.push(at(HOME, html, where, `feature has ${words} words, at most ${FEATURE_WORDS}`));
    }
    if (guides !== FEATURE_GUIDE_LINKS) {
      found.push(at(HOME, html, where, `feature has ${guides} guide links, not ${FEATURE_GUIDE_LINKS}`));
    }
  });
  return found;
}

// S14: Home's main at most 1,200 words outside pre.
function homeWords(site) {
  const found = [];
  const html = homeOf(site, found);
  if (html === null) {
    return found;
  }
  const main = mainOf(html);
  const words = wordCount(textOf(main.inner.replace(/<pre\b[\s\S]*?<\/pre>/gi, ' ')));
  if (words === 0 || words > HOME_WORDS) {
    found.push(at(HOME, html, main.start, `${words} words outside pre, 1 to ${HOME_WORDS}`));
  }
  return found;
}

// S6: a copy button with a name and a live region; the install line as text.
function copyControl(site) {
  const found = [];
  const html = homeOf(site, found);
  if (html === null) {
    return found;
  }
  const buttons = elementsOf(html, 'button').filter((b) => hasAttr(b.tag, 'data-copy'));
  if (buttons.length === 0) {
    found.push(`${HOME}:1: no button[data-copy]`);
  }
  for (const button of buttons.filter((b) => nameOf(b) === '')) {
    found.push(at(HOME, html, button.start, 'copy button without an accessible name'));
  }
  if (!/\saria-live="(polite|assertive)"/.test(html)) {
    found.push(`${HOME}:1: no aria-live region`);
  }
  if (!installText().test(html)) {
    found.push(`${HOME}:1: the install line is not text in the page`);
  }
  return found;
}

// S8: the architectures Home names equal those of out/install.sh's target triples, and Home
// names the platforms without a build.
function platforms(site) {
  const found = [];
  const html = homeOf(site, found);
  const script = path.join(site.out, 'install.sh');
  if (html === null || !existsSync(script)) {
    return [...found, 'install.sh:0: missing'];
  }
  const built = new Set([...readFileSync(script, 'utf8').matchAll(TRIPLE)].map((m) => m[1]));
  const text = textOf(mainOf(html).inner);
  const named = new Set([...text.matchAll(ARCH_WORDS)].map((m) => m[1]));
  const same = built.size > 0 && built.size === named.size && [...built].every((a) => named.has(a));
  if (!same) {
    found.push(`${HOME}:1: Home names ${[...named].join(', ') || 'no architecture'}; install.sh builds ${[...built].join(', ')}`);
  }
  for (const platform of UNSUPPORTED.filter((p) => !text.includes(p))) {
    found.push(`${HOME}:1: Home does not name ${platform} as unsupported`);
  }
  return found;
}

// S11: Home's MCP registration line occurs byte for byte in docs/develop/mcp.md.
function mcpCommand(site) {
  const found = [];
  const html = homeOf(site, found);
  const guide = path.join(site.repo, 'docs/develop/mcp.md');
  if (html === null || !existsSync(guide)) {
    return [...found, 'docs/develop/mcp.md:0: missing'];
  }
  const code = elementsOf(html, 'code').map((c) => textOf(c.inner.replace(/<br\s*\/?>|\n/g, '\u0000')));
  const lines = code.flatMap((c) => c.split('\u0000')).map((l) => l.trim()).filter((l) => l.includes(MCP_SERVER));
  if (lines.length === 0) {
    return [`${HOME}:1: no MCP registration command (a line with "${MCP_SERVER.trim()}")`];
  }
  const text = readFileSync(guide, 'utf8');
  for (const line of lines.filter((l) => !text.includes(l))) {
    found.push(`${HOME}:1: "${line}" is not in docs/develop/mcp.md`);
  }
  return found;
}

/** The latest released version of CHANGELOG.md as numbers, or null. */
function latestRelease(site) {
  const file = path.join(site.repo, 'CHANGELOG.md');
  const match = existsSync(file) ? RELEASED_HEADING.exec(readFileSync(file, 'utf8')) : null;
  return match ? match.slice(1, 4).map(Number) : null;
}

/** Whether version `a` is above `b`, both [major, minor, patch]. */
function above(a, b) {
  const i = a.findIndex((n, k) => n !== b[k]);
  return i !== -1 && a[i] > b[i];
}

// S17: a version in a sentence with "next" on a product page is above the latest release.
function nextVersion(site) {
  const found = [];
  const latest = latestRelease(site);
  if (latest === null) {
    return ['CHANGELOG.md:0: no released section'];
  }
  for (const file of needPages(productPages(site), 'product pages', found)) {
    const html = pageHtml(site, file);
    for (const block of blocksOf(html)) {
      const sentences = textOf(block.inner).split(/(?<=[.!?])\s+/);
      for (const sentence of sentences.filter((s) => NEXT_WORD.test(s))) {
        for (const m of sentence.matchAll(VERSION)) {
          if (!above(m.slice(1, 4).map(Number), latest)) {
            found.push(at(file, html, block.start, `"next" names ${m[0]}, not above ${latest.join('.')}`));
          }
        }
      }
    }
  }
  return found;
}

// S20: /product/vs-git opens by saying it is judged from the design with no benchmark; any
// product paragraph comparing with git says "from the design" or links the ledger.
function toolComparison(site) {
  const found = [];
  if (!existsSync(path.join(site.out, VS_GIT))) {
    return [`${VS_GIT}:0: missing`];
  }
  const vsGit = pageHtml(site, VS_GIT);
  const opening = elementsOf(mainOf(vsGit).inner, 'p')[0];
  const first = opening ? textOf(opening.inner) : '';
  if (!DESIGN_ONLY.test(first) || !NO_BENCHMARK.test(first)) {
    found.push(`${VS_GIT}:1: the first paragraph does not say "from the design" and "no benchmark"`);
  }
  for (const file of productPages(site)) {
    const html = pageHtml(site, file);
    for (const block of blocksOf(html)) {
      const text = textOf(block.inner);
      const compares = TOOL_NAME.test(text) && TOOL_COMPARE.test(text);
      if (compares && !DESIGN_ONLY.test(text) && !LEDGER_LINK.test(block.inner)) {
        found.push(at(file, html, block.start, `compares with git without "from the design" or a ledger link: ${text.slice(0, 80)}`));
      }
    }
  }
  return found;
}

// S4: a product paragraph with faster, slower, smaller or × links /docs/perf or /docs/goals.
function comparativeLink(site) {
  const found = [];
  for (const file of needPages(productPages(site), 'product pages', found)) {
    const html = pageHtml(site, file);
    for (const block of blocksOf(html)) {
      const text = textOf(block.inner);
      if (COMPARATIVE.test(text) && !LEDGER_LINK.test(block.inner)) {
        found.push(at(file, html, block.start, `comparative without a ledger link: ${text.slice(0, 80)}`));
      }
    }
  }
  return found;
}

// S4: no praise word on a product page.
function praiseWords(site) {
  const found = [];
  for (const file of needPages(productPages(site), 'product pages', found)) {
    const html = pageHtml(site, file);
    const main = mainOf(html);
    const text = blank(main.inner).replace(/<[^>]+>/g, (t) => t.replace(/[^\n]/g, ' '));
    const base = main.start + (main.tag?.length ?? 0);
    for (const m of text.matchAll(PRAISE_WORDS)) {
      found.push(at(file, html, base + m.index, `praise word "${m[0]}"`));
    }
  }
  return found;
}

// S21: every blockquote on a product page carries a link (in it or in its figcaption).
function blockquoteSource(site) {
  const found = [];
  for (const file of needPages(productPages(site), 'product pages', found)) {
    const html = pageHtml(site, file);
    for (const quote of elementsOf(html, 'blockquote')) {
      const caption = /^\s*<figcaption\b[\s\S]*?<\/figcaption>/i.exec(html.slice(quote.end))?.[0] ?? '';
      if (!/\shref="/.test(quote.inner + caption)) {
        found.push(at(file, html, quote.start, 'blockquote without a source link'));
      }
    }
  }
  return found;
}

// S35: every img has an alt (empty for decoration); every svg image a name.
function imageText(site) {
  const found = [];
  for (const file of needPages(pagesOf(site), 'pages', found)) {
    const html = pageHtml(site, file);
    for (const m of html.matchAll(/<img\b[^>]*>/gi)) {
      if (!hasAttr(m[0], 'alt')) {
        found.push(at(file, html, m.index, `img ${attrOf(m[0], 'src')} without alt`));
      }
    }
    for (const svg of elementsOf(html, 'svg').filter((s) => attrOf(s.tag, 'role') === 'img')) {
      if (!attrOf(svg.tag, 'aria-label') && !/<title\b/i.test(svg.inner)) {
        found.push(at(file, html, svg.start, 'svg image without a name'));
      }
    }
  }
  return found;
}

// S40: every link and button has an accessible name.
function names(site) {
  const found = [];
  for (const file of needPages(pagesOf(site), 'pages', found)) {
    const html = pageHtml(site, file);
    for (const element of [...elementsOf(html, 'a'), ...elementsOf(html, 'button')]) {
      if (nameOf(element) === '') {
        found.push(at(file, html, element.start, `${element.tag.slice(0, 60)} without a name`));
      }
    }
  }
  return found;
}

/** Own source files: web/app and web/components, repository-relative. */
function ownSources(site) {
  return SOURCE_DIRS.flatMap((dir) => walk(path.join(site.repo, dir), `${dir}/`))
    .filter((f) => SOURCE_EXT.test(f));
}

/** Each line of the own sources that matches, as findings. */
function scanSources(site, pattern, message) {
  const found = [];
  const sources = ownSources(site);
  if (sources.length === 0) {
    return ['web/app:0: no source file'];
  }
  for (const file of sources) {
    const lines = readFileSync(path.join(site.repo, file), 'utf8').split('\n');
    lines.forEach((line, i) => {
      const hits = typeof pattern === 'string'
        ? line.split(pattern).slice(1).map(() => pattern)
        : [...line.matchAll(pattern)].map((m) => m[0]);
      for (const hit of hits) {
        found.push(`${file}:${i + 1}: ${message} ${hit}`);
      }
    });
  }
  return found;
}

/** Milliseconds of a CSS time: `.12s` -> 120, `150ms` -> 150. */
function millis(value, unit) {
  return unit === 's' ? Number(value) * 1000 : Number(value);
}

// S34: the built rule of each own duration class at most 200 ms; no own animation; transitions
// in global.css at most 200 ms; the reduced-motion rule in the built CSS.
function motion(site) {
  const found = [];
  const css = walk(path.join(site.out, '_next')).filter((f) => f.endsWith('.css'));
  const built = css.map((f) => readFileSync(path.join(site.out, '_next', f), 'utf8')).join('\n');
  if (built === '') {
    return ['out/_next:0: no built CSS'];
  }
  if (!REDUCED_MOTION.test(built)) {
    found.push('out/_next:0: no reduced-motion rule that stops transitions');
  }

  // 1. Own duration classes, as the build compiled them.
  const classes = new Set(scanSources(site, OWN_DURATION, '').map((f) => f.split(': ').at(-1).trim()));
  for (const name of classes) {
    const escaped = name.replace(/[[\]().]/g, (c) => `\\\\\\${c}`);
    const rule = new RegExp(`\\.${escaped}\\{[^}]*transition-duration:([\\d.]+)(ms|s)`).exec(built);
    if (!rule) {
      found.push(`out/_next:0: .${name} not in the built CSS`);
    } else if (millis(rule[1], rule[2]) > MOTION_MAX_MS) {
      found.push(`out/_next:0: .${name} lasts ${millis(rule[1], rule[2])} ms, at most ${MOTION_MAX_MS}`);
    }
  }

  // 2. No own animation; global.css transitions within the limit.
  found.push(...scanSources(site, OWN_ANIMATE, 'animation class'));
  const global = path.join(site.repo, 'web/app/global.css');
  const lines = existsSync(global) ? readFileSync(global, 'utf8').split('\n') : [];
  lines.forEach((line, i) => {
    if (/(^|[\s;{])animation(-name)?\s*:/.test(line)) {
      found.push(`web/app/global.css:${i + 1}: animation`);
    }
    if (!/transition(-duration)?\s*:/.test(line)) {
      return;
    }
    for (const m of line.matchAll(/(\d*\.?\d+)(ms|s)\b/g)) {
      if (millis(m[1], m[2]) > MOTION_MAX_MS) {
        found.push(`web/app/global.css:${i + 1}: transition of ${m[0]}, at most ${MOTION_MAX_MS} ms`);
      }
    }
  });
  return found;
}

// S32: no literal type size.
function literalSize(site) {
  return scanSources(site, LITERAL_SIZE, 'literal size');
}

// S33: no pixel literal or half step for spacing.
function literalSpace(site) {
  return scanSources(site, LITERAL_SPACE, 'literal spacing');
}

/** The Markdown twins of the export: `/docs.md` and `/docs/**.md`, as site paths. */
function twinsOf(site) {
  const nested = walk(path.join(site.out, 'docs'), 'docs/').filter((f) => f.endsWith('.md'));
  const index = existsSync(path.join(site.out, 'docs.md')) ? ['docs.md'] : [];
  return [...index, ...nested].map((f) => `/${f}`);
}

// S16, D44: llms.txt in its format (h1, summary quote, h2 sections of link lists), every twin
// listed once, the agents page first, every link a twin.
function llms(site) {
  const file = path.join(site.out, 'llms.txt');
  if (!existsSync(file)) {
    return ['llms.txt:0: missing'];
  }
  const found = [];
  const lines = readFileSync(file, 'utf8').split('\n');
  if (!/^# \S/.test(lines[0] ?? '')) {
    found.push('llms.txt:1: does not open with "# Name"');
  }
  if (!lines.slice(1).find((l) => l.trim() !== '')?.startsWith('> ')) {
    found.push('llms.txt:2: no "> summary" after the title');
  }

  // Sections: after the first h2, a line is blank, a heading or a link line.
  const links = [];
  let inSection = false;
  lines.forEach((line, i) => {
    if (line.startsWith('## ')) {
      inSection = true;
      return;
    }
    if (!inSection || line.trim() === '') {
      return;
    }
    const link = /^- \[([^\]]+)\]\(([^)\s]+)\)(: .+)?$/.exec(line);
    if (!link) {
      found.push(`llms.txt:${i + 1}: not a "- [title](url): note" line`);
      return;
    }
    links.push({ line: i + 1, url: link[2] });
  });
  if (links.length === 0) {
    return [...found, 'llms.txt:0: no link'];
  }

  // Each link a twin, each twin once, the agents page first.
  const paths = links.map((l) => (l.url.startsWith(SITE_URL) ? l.url.slice(SITE_URL.length) : l.url));
  paths.forEach((p, i) => {
    if (!p.endsWith('.md') || servedIn(site, p) === null) {
      found.push(`llms.txt:${links[i].line}: ${links[i].url} is not a Markdown twin in out/`);
    }
  });
  for (const twin of twinsOf(site)) {
    const times = paths.filter((p) => p === twin).length;
    if (times !== 1) {
      found.push(`llms.txt:0: ${twin} listed ${times} times, not once`);
    }
  }
  if (paths[0] !== AGENTS_TWIN) {
    found.push(`llms.txt:${links[0].line}: first link ${paths[0]}, not ${AGENTS_TWIN}`);
  }
  return found;
}

/** Markdown link targets of a file outside fences: line and target. */
function markdownLinks(text) {
  const found = [];
  let fenced = false;
  text.split('\n').forEach((line, i) => {
    if (/^\s*(```|~~~)/.test(line)) {
      fenced = !fenced;
      return;
    }
    if (fenced) {
      return;
    }
    const plain = line.replace(/`[^`]*`/g, '');
    for (const m of plain.matchAll(/\]\(([^)\s]+)(?:\s+"[^"]*")?\)|^\s*\[[^\]]+\]:\s*(\S+)/g)) {
      found.push({ line: i + 1, target: m[1] ?? m[2] });
    }
  });
  return found;
}

// D45, D49: each docs page names its text/markdown alternate; each twin's relative links resolve.
function twinLinks(site) {
  const found = [];
  for (const file of needPages(docsPages(site), 'docs pages', found)) {
    const html = outText(site, file);
    const twin = (tag) => /\srel="alternate"/.test(tag) && /\stype="text\/markdown"/.test(tag);
    const tag = [...html.matchAll(/<link\b[^>]*>/gi)].map((m) => m[0]).find(twin);
    const href = tag ? attrOf(tag, 'href') : null;
    if (!href || servedIn(site, href) === null) {
      found.push(`${file}:1: no text/markdown alternate that resolves (${href})`);
    }
  }
  for (const twin of twinsOf(site)) {
    const text = readFileSync(path.join(site.out, twin), 'utf8');
    for (const link of markdownLinks(text)) {
      if (/^([a-z][a-z0-9+.-]*:|#|\/\/)/i.test(link.target)) {
        continue;
      }
      const target = link.target.split('#')[0];
      const resolved = path.posix.resolve(path.posix.dirname(twin), target);
      if (servedIn(site, resolved) === null) {
        found.push(`${twin.slice(1)}:${link.line}: ${link.target} resolves to nothing in out/`);
      }
    }
  }
  return found;
}

/** Every string of a JSON value. */
function stringsOf(value) {
  if (typeof value === 'string') {
    return [value];
  }
  if (value && typeof value === 'object') {
    return Object.values(value).flatMap(stringsOf);
  }
  return [];
}

/** The site paths of the pages in one export directory. `/docs/reference/commands/init`. */
function pagesIn(site, dir) {
  return walk(path.join(site.out, dir), `${dir}/`)
    .filter((f) => f.endsWith('.html'))
    .map((f) => `/${f.slice(0, -'.html'.length)}`);
}

// S24, D32: /api/lookup holds every command page, every MCP tool page and every E_* heading of
// the errors page; each of its addresses resolves.
function lookup(site) {
  const file = path.join(site.out, LOOKUP);
  if (!existsSync(file)) {
    return [`${LOOKUP}:0: missing`];
  }
  let table;
  try {
    table = JSON.parse(readFileSync(file, 'utf8'));
  } catch (error) {
    return [`${LOOKUP}:0: not JSON: ${error.message}`];
  }
  const addresses = stringsOf(table).filter((s) => s.startsWith('/'));
  const found = addresses.filter((a) => !resolves(site, a)).map((a) => `${LOOKUP}:0: ${a} does not resolve`);

  // What it must hold: commands, tools, error codes; none found is a failure.
  const errors = 'docs/reference/errors.html';
  const headingIds = existsSync(path.join(site.out, errors))
    ? [...outText(site, errors).matchAll(/<h[2-4]\b[^>]*\sid="([^"]+)"[^>]*>\s*E_[A-Z0-9_]+/g)].map((m) => m[1])
    : [];
  const codes = headingIds.map((id) => `/docs/reference/errors#${id}`);
  const wanted = { commands: pagesIn(site, 'docs/reference/commands'), tools: pagesIn(site, 'docs/reference/mcp-tools'), codes };
  const held = new Set(addresses);
  const pages = new Set(addresses.map((a) => a.split('#')[0]));
  for (const [kind, list] of Object.entries(wanted)) {
    if (list.length === 0) {
      found.push(`${LOOKUP}:0: no ${kind} found in out/`);
    }
    const has = kind === 'codes' ? held : pages;
    for (const address of list.filter((a) => !has.has(a))) {
      found.push(`${LOOKUP}:0: lacks ${address}`);
    }
  }
  return found;
}

/** The page addresses of the export: `/`, `/docs/perf`, ... sorted. */
function exportAddresses(site) {
  return walk(site.out)
    .filter((f) => f.endsWith('.html') && !NOT_PAGES.includes(f))
    .map((f) => `/${f.slice(0, -'.html'.length)}`.replace(/(^|\/)index$/, '$1'))
    .map((a) => (a === '' ? '/' : a))
    .toSorted();
}

/** The source addresses of _redirects. */
function redirectSources(site) {
  const file = path.join(site.out, '_redirects');
  const lines = existsSync(file) ? readFileSync(file, 'utf8').split('\n') : [];
  return lines.filter((l) => l.trim() && !l.startsWith('#')).map((l) => l.trim().split(/\s+/)[0]);
}

// D18: every line of web/addresses.txt is a page in out/ or the source of a redirect.
function addresses(site) {
  const file = path.join(site.repo, ADDRESSES);
  const lines = existsSync(file) ? readFileSync(file, 'utf8').split('\n') : [];
  const listed = lines.map((l, i) => ({ line: i + 1, address: l.trim() })).filter((l) => l.address && !l.address.startsWith('#'));
  if (listed.length === 0) {
    return [`${ADDRESSES}:0: missing or empty`];
  }
  const redirects = redirectSources(site);
  const redirected = (a) => redirects.some((r) => r === a || (r.endsWith('*') && a.startsWith(r.slice(0, -1))));
  return listed
    .filter((l) => servedIn(site, l.address) === null && !redirected(l.address))
    .map((l) => `${ADDRESSES}:${l.line}: ${l.address} is no page and no redirect source`);
}

// S48: README's install line, version and status word equal Home's.
function readmeHomeDrift(site) {
  const found = [];
  const html = homeOf(site, found);
  const file = path.join(site.repo, 'README.md');
  if (html === null || !existsSync(file)) {
    return [...found, 'README.md:0: missing'];
  }
  const readme = readFileSync(file, 'utf8');
  const prose = readme.replace(/^(```|~~~)[\s\S]*?^\1/gm, ' ');
  // Home's text without comments: `v<!-- -->0.11.0` reads `v0.11.0`, as a browser renders it.
  const raw = outText(site, HOME).replace(/<!--[\s\S]*?-->/g, '');
  const home = textOf(mainOf(raw).inner);

  // 1. The install line, 2. the version, 3. the status word.
  const install = readme.split('\n').find((l) => l.includes('get.swarmops.cc/install.sh'))?.trim();
  if (!install || !home.includes(install)) {
    found.push(`README.md:0: install line "${install}" not on Home`);
  }
  const pairs = [
    ['version', README_VERSION.exec(prose)?.[1], README_VERSION.exec(home)?.[1]],
    ['status word', STATUS_WORD.exec(prose)?.[1]?.toLowerCase(), STATUS_WORD.exec(home)?.[1]?.toLowerCase()],
  ];
  for (const [what, ours, theirs] of pairs) {
    if (!ours || ours !== theirs) {
      found.push(`README.md:0: ${what} "${ours}" differs from Home's "${theirs}"`);
    }
  }
  return found;
}

/** Gzip size of an export file, in bytes; 0 when it is not in out/. */
function gzipOf(site, rel) {
  const file = path.join(site.out, rel.split(/[?#]/)[0]);
  return existsSync(file) && statSync(file).isFile() ? gzipSync(readFileSync(file)).length : 0;
}

// S38: Home's transfer on first visit, gzip: the page, its scripts (not noModule), stylesheets,
// icons and images. Reported; above 300 kB fails.
function homeTransfer(site) {
  const found = [];
  if (homeOf(site, found) === null) {
    return found;
  }
  const html = outText(site, HOME);
  const scripts = [...html.matchAll(/<script([^>]*)\ssrc="(\/[^"]+)"([^>]*)>/g)]
    .filter((m) => !/\snoModule/i.test(m[1] + m[3]))
    .map((m) => m[2]);
  const links = [...html.matchAll(/<link\b[^>]*>/g)]
    .filter((m) => /\srel="(stylesheet|icon)"/.test(m[0]))
    .map((m) => attrOf(m[0], 'href'));
  const images = [...html.matchAll(/<img\b[^>]*\ssrc="(\/[^"]+)"/g)].map((m) => m[1]);
  const kb = (list) => [...new Set(list)].reduce((sum, rel) => sum + gzipOf(site, rel), 0) / 1024;

  const script = kb(scripts);
  const total = gzipSync(html).length / 1024 + script + kb([...links, ...images]);
  site.notes.push(`Home transfer (gzip): ${total.toFixed(1)} kB total, ${script.toFixed(1)} kB script (limit ${HOME_TRANSFER_KB} kB total)`);
  if (total > HOME_TRANSFER_KB) {
    found.push(`${HOME}:1: ${total.toFixed(1)} kB transferred (gzip), at most ${HOME_TRANSFER_KB}`);
  }
  return found;
}

// Every rule by the name the design gives it.
const RULES = [
  ['structure', structure],
  ['header-links', headerLinks],
  ['footer-map', footerMap],
  ['release-links', releaseLinks],
  ['home-first-screen', homeFirstScreen],
  ['home-primary', homePrimary],
  ['home-order', homeOrder],
  ['feature-words', featureWords],
  ['home-words', homeWords],
  ['copy-control', copyControl],
  ['platforms', platforms],
  ['mcp-command', mcpCommand],
  ['next-version', nextVersion],
  ['tool-comparison', toolComparison],
  ['comparative-link', comparativeLink],
  ['praise-words', praiseWords],
  ['blockquote-source', blockquoteSource],
  ['image-text', imageText],
  ['names', names],
  ['motion', motion],
  ['literal-size', literalSize],
  ['literal-space', literalSpace],
  ['llms', llms],
  ['twin-links', twinLinks],
  ['lookup', lookup],
  ['addresses', addresses],
  ['readme-home-drift', readmeHomeDrift],
  ['home-transfer', homeTransfer],
];

// The rules the built tree passes; each other rule reports PENDING until a fix slice clears it,
// then joins this list. None leaves it.
const ENFORCED = new Set([
  'header-links',
  'lookup',
  'literal-size',
  'literal-space',
  'home-primary',
  'footer-map',
  'release-links',
  'home-order',
  'feature-words',
  'copy-control',
  'platforms',
  'mcp-command',
  'praise-words',
  'blockquote-source',
  'image-text',
  'names',
  'motion',
  'llms',
  'addresses',
  'readme-home-drift',
  'home-transfer',
]);

/** Every rule over one site root: name -> findings. */
function runRules(site, only = null) {
  const results = new Map();
  for (const [name, rule] of RULES) {
    if (only === null || only === name) {
      results.set(name, rule(site));
    }
  }
  return results;
}

/** Rewrites web/addresses.txt from the export. */
function writeAddresses() {
  const site = siteAt(REPO);
  const list = exportAddresses(site);
  if (list.length === 0) {
    console.error('check-site: no page in out/; run `npm run build` first');
    process.exit(1);
  }
  writeFileSync(path.join(REPO, ADDRESSES), `${list.join('\n')}\n`);
  console.log(`check-site: ${ADDRESSES}: ${list.length} addresses`);
}

// ---------------------------------------------------------------------------------------------
// Selftest: a fixture site that passes every rule, then one planted violation per rule, each of
// which its rule must report.

const FX_RELEASE = '0.11.0';
const FX_MCP = `client mcp add swarmops -e SWARMOPS_AGENT=agent/a${MCP_SERVER} --repo /tmp/acme`;
const FX_SKIP = '<a class="skip-link" href="#main">Skip to content</a>';
const FX_NAV = [
  ['/docs', 'Docs'],
  ['/product', 'Product'],
  ['/roadmap', 'Roadmap'],
  ['/docs/changelog', 'Changelog'],
  [REPO_URL, 'GitHub'],
];

/** A fixture page in the site's frame. */
function fxPage(title, body, head = '') {
  const nav = FX_NAV.map(([href, text]) => `<a href="${href}">${text}</a>`).join('');
  const footer = FOOTER_MAP.map((href, i) => `<a href="${href}">Destination ${i + 1}</a>`).join('');
  return [
    `<!DOCTYPE html><html lang="en"><head><title>${title}</title>${head}`,
    '<link rel="stylesheet" href="/_next/static/chunks/a.css"/></head>',
    `<body><div hidden=""></div><script>document.documentElement.dataset.a="<a>"</script>${FX_SKIP}`,
    `<header><a href="/"><img src="/logo.svg" alt=""/>SwarmOps</a><nav aria-label="Sections">${nav}</nav>`,
    `<a href="/docs/changelog#${FX_RELEASE.replaceAll('.', '')}">v${FX_RELEASE}</a></header>`,
    `<main id="main"><h1>${title}</h1>\n${body}\n</main><footer><h2>Site</h2>${footer}</footer></body></html>\n`,
  ].join('');
}

/** A feature of the fixture Home. */
function fxFeature(name) {
  return `<h3>${name}</h3><p>Fifty agents commit at once. <a href="/docs/guides/${name}">Read the guide</a></p>`;
}

/** A docs page and its twin. */
function fxDoc(files, slug, title, body = '', twin = `# ${title}\n`) {
  const html = slug === '' ? 'docs.html' : `docs/${slug}.html`;
  const md = `/${html.slice(0, -'.html'.length)}.md`;
  files.set(`web/out/${html}`, fxPage(title, body, `<link rel="alternate" type="text/markdown" href="${md}"/>`));
  files.set(`web/out${md}`, twin);
}

/** The fixture tree: repository-relative path -> text. */
function fixture() {
  const files = new Map();
  const home = [
    '<p>Many agent processes commit into one store at once.</p>',
    // React splits adjacent text nodes with an empty comment: `v<!-- -->0.11.0`.
    `<p>Pre-release, v<!-- -->${FX_RELEASE}. Linux x86_64 and aarch64; no macOS or Windows build yet.</p>`,
    '<p>Measured on v0.6.0.</p>',
    `<div><code>${INSTALL_LINE}</code><button type="button" data-copy="${INSTALL_LINE}" aria-label="Copy the install line">Copy</button><span aria-live="polite"></span></div>`,
    '<a data-primary="" href="/docs/guides/quick-start">Quick start</a>',
    `<section id="first-run"><h2>First commands</h2><pre><code>${FX_MCP}</code></pre></section>`,
    '<section id="sample"><h2>Sample</h2><pre>swarmops init</pre></section>',
    `<section id="features"><h2>Features</h2>${fxFeature('commit-and-land')}${fxFeature('parallel-agents')}</section>`,
    '<section id="evidence"><h2>Evidence</h2><p>1 met.</p></section>',
    '<section id="start"><h2>Start</h2><p>Begin here. <a data-primary="" href="/docs/guides/quick-start">Quick start</a></p></section>',
  ].join('\n');
  files.set(`web/out/${HOME}`, fxPage('Version control for swarms of coding agents', home));
  files.set(`web/out/${VS_GIT}`, fxPage('SwarmOps and git', [
    '<p>This comparison is judged from the design; no benchmark against git exists.</p>',
    '<blockquote><p>It held.</p><a href="/docs/pilot">The pilot</a></blockquote>',
    '<p>Lands are 2× faster, as <a href="/docs/perf#lands">the ledger</a> records.</p>',
    '<p>The next release is 0.12.0.</p>',
  ].join('\n')));

  // Docs pages, their twins, the lookup table and llms.txt.
  fxDoc(files, '', 'Documentation', '', '# Documentation\n\nSee [the agents page](docs/agents.md).\n');
  fxDoc(files, 'agents', 'Agents', '', '# Agents\n\nSee [errors](reference/errors.md#e_busy) and [the site](https://swarmops.cc).\n');
  fxDoc(files, 'reference/commands/init', 'init');
  fxDoc(files, 'reference/mcp-tools/status', 'status');
  fxDoc(files, 'reference/errors', 'Errors', '<h2 id="e_busy">E_BUSY</h2><p>Busy.</p>');
  const twins = ['docs/agents', 'docs', 'docs/reference/commands/init', 'docs/reference/mcp-tools/status', 'docs/reference/errors'];
  const list = twins.map((t) => `- [${t}](${SITE_URL}/${t}.md): one line.`).join('\n');
  files.set('web/out/llms.txt', `# SwarmOps\n\n> Version control for swarms of coding agents.\n\n## Docs\n\n${list}\n\n## Optional\n`);
  files.set(`web/out/${LOOKUP}`, JSON.stringify([
    { name: 'init', url: '/docs/reference/commands/init' },
    { name: 'status', url: '/docs/reference/mcp-tools/status' },
    { name: 'E_BUSY', url: '/docs/reference/errors#e_busy' },
  ]));

  // Served files, built CSS, own sources, repository files.
  files.set('web/out/install.sh', 'target=x86_64-unknown-linux-gnu\ntarget=aarch64-unknown-linux-gnu\n');
  files.set('web/out/_redirects', '# moved\n/guides/old /docs 308\n');
  files.set('web/out/logo.svg', '<svg xmlns="http://www.w3.org/2000/svg"/>\n');
  files.set('web/out/_next/static/chunks/a.css', [
    '.duration-120{--tw-duration:.12s;transition-duration:.12s}',
    '@media (prefers-reduced-motion:reduce){*,:before,:after{transition-duration:.01ms!important}}\n',
  ].join(''));
  files.set('web/components/link.tsx', 'export const LINK = "px-4 py-2 text-sm transition-colors duration-120";\n');
  files.set('web/app/global.css', ':root {\n  --radius: 0.5rem;\n}\na {\n  transition: color 120ms;\n}\n');
  files.set(ADDRESSES, '/\n/product/vs-git\n/docs\n/docs/agents\n/guides/old\n');
  files.set('docs/develop/mcp.md', `# MCP\n\n\`\`\`sh\n${FX_MCP}\n\`\`\`\n`);
  files.set('README.md', `# SwarmOps\n\nPre-release, v${FX_RELEASE}.\n\n\`\`\`sh\n${INSTALL_LINE}\n\`\`\`\n`);
  files.set('CHANGELOG.md', `# Changelog\n\n## [Unreleased]\n\n## [${FX_RELEASE}] - 2026-11-09\n`);
  return files;
}

/** Replaces text in one fixture file; a replacement that matches nothing is a selftest bug. */
function edit(files, rel, from, to) {
  const text = files.get(rel);
  if (!text.includes(from)) {
    throw new Error(`selftest: ${rel} has no "${from}"`);
  }
  files.set(rel, text.replace(from, to));
}

const OUT_HOME = `web/out/${HOME}`;
const OUT_VS_GIT = `web/out/${VS_GIT}`;
const FX_SOURCE = 'web/components/link.tsx';

// One planted violation per rule.
const MUTATIONS = {
  'structure': (f) => edit(f, OUT_HOME, FX_SKIP, ''),
  'header-links': (f) => edit(f, OUT_VS_GIT, '</nav>', '<a href="/a">A</a><a href="/docs">Docs again</a></nav>'),
  'footer-map': (f) => edit(f, OUT_VS_GIT, `<a href="${REPO_URL}/issues/new/choose">`, '<a href="/elsewhere">'),
  'release-links': (f) => edit(f, OUT_VS_GIT, '</main>', `<p><a href="${REPO_URL}/releases">Releases</a></p></main>`),
  'home-first-screen': (f) => edit(f, OUT_HOME, '<h1>Version control', '<h1>The one and only version control system built'),
  'home-primary': (f) => edit(f, OUT_HOME, 'Begin here. <a data-primary="" ', 'Begin here. <a '),
  'home-order': (f) => edit(f, OUT_HOME, 'id="sample"', 'id="later"'),
  'feature-words': (f) => edit(f, OUT_HOME, 'Fifty agents commit at once.', 'Fifty agents <a href="/docs/perf">commit</a> at once.'),
  'home-words': (f) => edit(f, OUT_HOME, '<p>1 met.</p>', `<p>${'word '.repeat(HOME_WORDS)}</p>`),
  'copy-control': (f) => edit(f, OUT_HOME, ' aria-live="polite"', ''),
  'platforms': (f) => edit(f, OUT_HOME, 'x86_64 and aarch64', 'x86_64, riscv64 and aarch64'),
  'mcp-command': (f) => edit(f, OUT_HOME, '--repo /tmp/acme', '--repo /tmp/other'),
  'next-version': (f) => edit(f, OUT_VS_GIT, 'is 0.12.0', 'is 0.9.0'),
  'tool-comparison': (f) => edit(f, OUT_VS_GIT, 'judged from the design; no benchmark against git exists', 'SwarmOps lands faster than git'),
  'comparative-link': (f) => edit(f, OUT_VS_GIT, '<a href="/docs/perf#lands">the ledger</a>', 'the ledger'),
  'praise-words': (f) => edit(f, OUT_VS_GIT, 'It held.', 'It held, blazing.'),
  'blockquote-source': (f) => edit(f, OUT_VS_GIT, '<a href="/docs/pilot">The pilot</a>', 'The pilot'),
  'image-text': (f) => edit(f, OUT_HOME, '<h1>', '<img src="/logo.svg"/><h1>'),
  'names': (f) => edit(f, OUT_HOME, '</main>', '<button type="button"><svg aria-hidden="true"></svg></button></main>'),
  'motion': (f) => {
    edit(f, FX_SOURCE, 'duration-120', 'duration-300');
    edit(f, 'web/out/_next/static/chunks/a.css', '@media', '.duration-300{--tw-duration:.3s;transition-duration:.3s}@media');
  },
  'literal-size': (f) => edit(f, FX_SOURCE, 'text-sm', 'text-[15px]'),
  'literal-space': (f) => edit(f, FX_SOURCE, 'px-4', 'px-4 mt-2.5'),
  'llms': (f) => edit(f, 'web/out/llms.txt', '- [docs/agents]', '  - [docs/agents]'),
  'twin-links': (f) => edit(f, 'web/out/docs/agents.md', 'reference/errors.md', 'reference/missing.md'),
  'lookup': (f) => edit(f, `web/out/${LOOKUP}`, ',{"name":"E_BUSY","url":"/docs/reference/errors#e_busy"}', ''),
  'addresses': (f) => edit(f, ADDRESSES, '/guides/old\n', '/guides/old\n/gone\n'),
  'readme-home-drift': (f) => edit(f, 'README.md', `v${FX_RELEASE}`, 'v0.10.0'),
  'home-transfer': (f) => {
    edit(f, OUT_HOME, '</body>', '<script src="/_next/static/chunks/big.js"></script></body>');
    f.set('web/out/_next/static/chunks/big.js', randomBytes(HOME_TRANSFER_KB * 1024 * 2).toString('base64'));
  },
};

/** Writes a fixture tree into a fresh temporary directory and runs the rules over it. */
function runFixture(files, only = null) {
  const root = mkdtempSync(path.join(tmpdir(), 'check-site-'));
  try {
    for (const [rel, text] of files) {
      mkdirSync(path.dirname(path.join(root, rel)), { recursive: true });
      writeFileSync(path.join(root, rel), text);
    }
    return runRules(siteAt(root), only);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

function selftest() {
  const problems = [];

  // 1. The clean fixture passes every rule: no rule fails on everything.
  for (const [name, findings] of runFixture(fixture())) {
    for (const finding of findings) {
      problems.push(`clean fixture fails ${name}: ${finding}`);
    }
  }

  // 2. Each planted violation is reported by its rule.
  for (const [name] of RULES) {
    const mutate = MUTATIONS[name];
    if (!mutate) {
      problems.push(`${name}: no planted violation`);
      continue;
    }
    const files = fixture();
    mutate(files);
    const findings = runFixture(files, name).get(name);
    if (findings.length === 0) {
      problems.push(`${name}: the planted violation is not reported`);
      continue;
    }
    console.log(`check-site: selftest ${name} finds ${findings[0].slice(0, 110)}`);
  }

  for (const problem of problems) {
    console.error(`check-site: selftest FAIL ${problem}`);
  }
  console.log(`check-site: selftest ${problems.length === 0 ? 'ok' : `${problems.length} failures`}, ${RULES.length} rules`);
  process.exit(problems.length === 0 ? 0 : 1);
}

/** The rules over out/: enforced findings join `failures`, the rest print as PENDING. */
function checkRules(showAll) {
  const site = siteAt(REPO);
  const pending = [];
  for (const [name, findings] of runRules(site)) {
    if (ENFORCED.has(name)) {
      findings.forEach((f) => fail(`${name}: ${f}`));
      continue;
    }
    const shown = showAll ? findings : findings.slice(0, PENDING_SHOWN);
    shown.forEach((f) => console.log(`check-site: PENDING ${name}: ${f}`));
    if (shown.length < findings.length) {
      console.log(`check-site: PENDING ${name}: ... ${findings.length - shown.length} more (--pending lists all)`);
    }
    if (findings.length > 0) {
      pending.push(`${name} (${findings.length})`);
    }
  }
  site.notes.forEach((n) => console.log(`check-site: ${n}`));
  console.log(`check-site: rules enforced ${ENFORCED.size} of ${RULES.length}; pending with findings: ${pending.join(', ') || 'none'}`);
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
  checkRules(process.argv.includes('--pending'));

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

// Mode: the selftest, the address list, or the checks.
if (process.argv.includes('--selftest')) {
  selftest();
} else if (process.argv.includes('--write-addresses')) {
  writeAddresses();
} else {
  main();
}

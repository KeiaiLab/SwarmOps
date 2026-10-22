// After `next build`: brings the repository files the site serves into out/, and writes each
// page's Content-Security-Policy.
//
//   scripts/install.sh       -> out/install.sh            (get.swarmops.cc/install.sh)
//   docs/assets/             -> out/assets/docs/           (images the docs link to)
//
// The icons, the manifest and /explainers/ are files in public/, which `next build` copies.
//
// CSP: a static export cannot use nonces, so every page gets a <meta http-equiv> policy that
// allows its own inline scripts by sha256 and nothing else inline. _headers keeps a looser
// site-wide header ('unsafe-inline' for scripts); a browser enforces both, so the meta policy binds.
import { createHash } from 'node:crypto';
import { cpSync, existsSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import * as path from 'node:path';

const WEB = path.resolve(import.meta.dirname, '..');
const REPO = path.resolve(WEB, '..');
const OUT = path.join(WEB, 'out');

// Pages with their own policy in _headers (inline script by design), left as they are.
const OWN_POLICY_DIRS = ['explainers/'];

// Everything but scripts: styles allow inline (Shiki colours and layout set style attributes,
// which no hash can allow); connect-src 'self' for the search index and page data.
const POLICY = [
  "default-src 'none'",
  "style-src 'self' 'unsafe-inline'",
  "img-src 'self' data:",
  "font-src 'self'",
  "connect-src 'self'",
  "manifest-src 'self'",
  "base-uri 'none'",
  "form-action 'none'",
];

const CSP_META = /<meta http-equiv="Content-Security-Policy"[^>]*>/;
const INLINE_SCRIPT = /<script(?![^>]*\ssrc=)[^>]*>([\s\S]*?)<\/script>/g;

/** The base64 sha256 of a script's text, as CSP hashes it. */
function sha256(text) {
  return createHash('sha256')
    .update(text, 'utf8')
    .digest('base64');
}

/** Every file under `dir`, as paths relative to it. */
function walk(dir, prefix = '') {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const rel = prefix + entry.name;
    return entry.isDirectory() ? walk(path.join(dir, entry.name), `${rel}/`) : [rel];
  });
}

/** Copies a file or directory, failing on a missing source: a partial copy looks whole. */
function copy(from, to) {
  if (!existsSync(from)) {
    throw new Error(`postbuild: missing ${path.relative(REPO, from)}`);
  }
  cpSync(from, to, { recursive: true });
}

/** The policy of one page: its inline scripts by hash. */
function policyFor(html) {
  const hashes = [...html.matchAll(INLINE_SCRIPT)].map(
    ([, body]) => `'sha256-${sha256(body)}'`,
  );
  const scripts = ["script-src 'self'", ...new Set(hashes)].join(' ');
  return [scripts, ...POLICY].join('; ');
}

function main() {
  // 1. Repository files served beside the pages.
  copy(path.join(REPO, 'scripts/install.sh'), path.join(OUT, 'install.sh'));
  copy(path.join(REPO, 'docs/assets'), path.join(OUT, 'assets/docs'));

  // 2. One policy per page, first in <head> so it covers every script after it.
  const pages = walk(OUT).filter(
    (file) => file.endsWith('.html') && !OWN_POLICY_DIRS.some((dir) => file.startsWith(dir)),
  );
  for (const page of pages) {
    const file = path.join(OUT, page);
    const html = readFileSync(file, 'utf8').replace(CSP_META, '');
    const meta = `<meta http-equiv="Content-Security-Policy" content="${policyFor(html)}">`;
    if (!html.includes('<head>')) {
      throw new Error(`postbuild: ${page} has no <head>`);
    }
    writeFileSync(file, html.replace('<head>', `<head>${meta}`));
  }

  console.log(`postbuild: 2 copies, ${pages.length} pages with a CSP meta policy`);
}

main();

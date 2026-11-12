// Search basics of /now, read from the built site (run `just web-build` first):
//
//   node --experimental-strip-types --test 'web/test/*.test.ts'
//
// /now has a unique title, a description, canonical /now and Open Graph like the other pages.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import * as path from 'node:path';
import { test } from 'node:test';

const OUT = path.join(import.meta.dirname, '..', 'out');
const SITE_URL = 'https://swarmops.cc';
const NOW_TITLE = 'Now on stable: SwarmOps';

const nowHtml = readFileSync(path.join(OUT, 'now.html'), 'utf8');

/** The content of `<meta {attr}="{key}" content="...">`, or undefined. */
function meta(html: string, attr: string, key: string): string | undefined {
  const re = new RegExp(`<meta ${attr}="${key}" content="([^"]*)"`);
  return re.exec(html)?.[1];
}

test('now_title_unique', () => {
  // 1. The title stated once, without the site-wide template suffix.
  assert.match(nowHtml, new RegExp(`<title>${NOW_TITLE}</title>`));

  // 2. No other page carries it.
  const roadmap = readFileSync(path.join(OUT, 'roadmap.html'), 'utf8');
  assert.doesNotMatch(roadmap, new RegExp(`<title>${NOW_TITLE}</title>`));
});

test('now_description_from_facts', () => {
  // The description names the commit built (its short sha from the page's own attribute).
  const commit = /data-stable-commit="([0-9a-f]{40})"/.exec(nowHtml)?.[1];
  assert.ok(commit, 'now.html: no data-stable-commit');
  const description = meta(nowHtml, 'name', 'description');
  assert.ok(description, 'now.html: no description');
  assert.ok(description.includes(commit.slice(0, 8)), `description lacks the commit: ${description}`);
});

test('now_canonical_and_open_graph', () => {
  // 1. Canonical /now.
  assert.match(nowHtml, new RegExp(`<link rel="canonical" href="${SITE_URL}/now"/>`));

  // 2. Open Graph as the other pages: the site's image and name, the page's own title.
  assert.equal(meta(nowHtml, 'property', 'og:title'), NOW_TITLE);
  assert.equal(meta(nowHtml, 'property', 'og:site_name'), 'SwarmOps');
  assert.equal(meta(nowHtml, 'property', 'og:image'), `${SITE_URL}/og-image.png`);
  assert.ok(meta(nowHtml, 'property', 'og:description'), 'now.html: no og:description');
});

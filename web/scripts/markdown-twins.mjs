// After `next build` (run from `postbuild`): a Markdown twin beside every docs page, and llms.txt
// pointing at the twins.
//
//   out/docs/guides/install.html  <a href="/docs/guides/install.md"
//                                    data-markdown-source="docs/guides/install.md">
//   docs/guides/install.md     -> out/docs/guides/install.md      (served as text/markdown, _headers)
//   llms.txt  [Install](https://swarmops.cc/docs/guides/install)
//          -> [Install](https://swarmops.cc/docs/guides/install.md)
//
// The pages name their own source (components/view-markdown.tsx), so this script repeats none of
// the path rules of lib/docs-paths.ts. A page without exactly one such link fails the build,
// except the generated landing page, which has no source file.
import { copyFileSync, existsSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import * as path from 'node:path';

const WEB = path.resolve(import.meta.dirname, '..');
const REPO = path.resolve(WEB, '..');
const OUT = path.join(WEB, 'out');

const DOCS_INDEX = 'docs.html';
const DOCS_DIR = 'docs/';
const HTML_EXT = '.html';
const MARKDOWN_EXT = '.md';
const LLMS = 'llms.txt';

// Repository files a twin may copy: the docs tree and the changelog, nothing else.
const SOURCES = /^(docs\/.+\.md|CHANGELOG\.md)$/;

// The "View as Markdown" link: its tag, then the two attributes read from it.
const LINK_TAG = /<a\s[^>]*data-markdown-source="[^"]*"[^>]*>/g;
const HREF = /\shref="([^"]*)"/;
const SOURCE = /\sdata-markdown-source="([^"]*)"/;

// An absolute docs link in llms.txt: `](https://host/docs/path)`.
const LLMS_DOCS_LINK = /\]\((https?:\/\/[^/)\s]+)(\/docs(?:\/[^)#\s]*)?)\)/g;

/** Every file under `dir`, as paths relative to it. */
function walk(dir, prefix = '') {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const rel = prefix + entry.name;
    return entry.isDirectory() ? walk(path.join(dir, entry.name), `${rel}/`) : [rel];
  });
}

/** The docs pages of the export: `docs.html` and everything under `docs/`. */
function docsPages() {
  const nested = walk(path.join(OUT, DOCS_DIR), DOCS_DIR)
    .filter((file) => file.endsWith(HTML_EXT));
  return [DOCS_INDEX, ...nested].filter((file) => existsSync(path.join(OUT, file)));
}

/** The twin of one page: where it goes in out/ and which repository file it copies, or null. */
function twinOf(page) {
  const html = readFileSync(path.join(OUT, page), 'utf8');

  // 1. Exactly one link per page; the generated landing (no docs/README.md) has no source.
  const tags = html.match(LINK_TAG) ?? [];
  if (tags.length === 0 && page === DOCS_INDEX) {
    return null;
  }
  if (tags.length !== 1) {
    throw new Error(`markdown-twins: ${page} has ${tags.length} "View as Markdown" links, not 1`);
  }
  const href = HREF.exec(tags[0])?.[1];
  const source = SOURCE.exec(tags[0])?.[1];

  // 2. The twin sits beside the page: docs/a/b.html <-> /docs/a/b.md.
  const beside = `/${page.slice(0, -HTML_EXT.length)}${MARKDOWN_EXT}`;
  if (href !== beside) {
    throw new Error(`markdown-twins: ${page} links to ${href}, expected ${beside}`);
  }

  // 3. The source is a docs file of the repository.
  if (!source || !SOURCES.test(source) || source.split('/').includes('..')) {
    throw new Error(`markdown-twins: ${page} names source ${source}, not a docs file`);
  }
  const from = path.join(REPO, source);
  if (!existsSync(from) || !statSync(from).isFile()) {
    throw new Error(`markdown-twins: ${page} names source ${source}, which does not exist`);
  }
  return { url: href.slice(0, -MARKDOWN_EXT.length), from, to: path.join(OUT, href.slice(1)) };
}

/** Points every docs link of llms.txt at its twin; a docs link without one fails. */
function rewriteLlms(urls) {
  const file = path.join(OUT, LLMS);
  if (!existsSync(file)) {
    throw new Error(`markdown-twins: out/${LLMS} missing`);
  }

  let rewritten = 0;
  const text = readFileSync(file, 'utf8').replace(LLMS_DOCS_LINK, (link, origin, url) => {
    // A second run finds the links already pointing at twins.
    if (url.endsWith(MARKDOWN_EXT) && urls.has(url.slice(0, -MARKDOWN_EXT.length))) {
      rewritten += 1;
      return link;
    }
    if (!urls.has(url)) {
      throw new Error(`markdown-twins: ${LLMS} links to ${url}, which has no twin`);
    }
    rewritten += 1;
    return `](${origin}${url}${MARKDOWN_EXT})`;
  });
  writeFileSync(file, text);
  return rewritten;
}

function main() {
  // 1. Read every page's twin first, so a bad page leaves out/ untouched.
  const twins = docsPages()
    .map((page) => twinOf(page))
    .filter((twin) => twin !== null);
  if (twins.length === 0) {
    throw new Error('markdown-twins: no docs pages in out/; run `next build` first');
  }

  // 2. Copy each source beside its page.
  for (const twin of twins) {
    copyFileSync(twin.from, twin.to);
  }

  // 3. llms.txt lists the twins.
  const links = rewriteLlms(new Set(twins.map((twin) => twin.url)));

  console.log(`markdown-twins: ${twins.length} Markdown twins, ${links} ${LLMS} links to them`);
}

main();

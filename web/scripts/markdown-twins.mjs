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
//
// Each twin opens with the page's title and one line of facts read from the built page (the
// `swarmops:release` meta tag and the page-meta line's data attributes), then the file's text:
//
//   # Install
//
//   > Release: 0.11.0 · Last changed: 2026-11-07 · Source: https://github.com/.../install.md
//
//   ...the rest of docs/guides/install.md, unchanged...
import { existsSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
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

// The facts of the twin's header: the release meta tag, then the page-meta line's attributes.
const RELEASE_META = /<meta\s+name="swarmops:release"\s+content="([^"]*)"/;
const META_TAG = /<[a-z]+\s[^>]*data-page-meta="[^"]*"[^>]*>/;
const LAST_CHANGED = /\sdata-last-changed="([^"]*)"/;
const SOURCE_URL = /\sdata-source-url="([^"]*)"/;

// The source's title line: the first `# ` heading; what comes before it (a generator's comment)
// is not part of the page.
const TITLE_LINE = /^# .+$/m;

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
  return { url: href.slice(0, -MARKDOWN_EXT.length), from, to: path.join(OUT, href.slice(1)), facts: factsOf(page, html) };
}

/** `&amp;` and its kin back to text: attribute values come HTML-escaped. */
function unescapeHtml(text) {
  return text
    .replaceAll('&quot;', '"')
    .replaceAll('&#x27;', "'")
    .replaceAll('&#39;', "'")
    .replaceAll('&lt;', '<')
    .replaceAll('&gt;', '>')
    .replaceAll('&amp;', '&');
}

/** The release, last-change date ('' for a file git does not track yet) and source URL of a page. */
function factsOf(page, html) {
  const release = RELEASE_META.exec(html)?.[1];
  const tag = META_TAG.exec(html)?.[0];
  const sourceUrl = tag ? SOURCE_URL.exec(tag)?.[1] : undefined;
  if (!release || !tag || !sourceUrl) {
    throw new Error(`markdown-twins: ${page} lacks the release meta tag or the page-meta line`);
  }
  return {
    release: unescapeHtml(release),
    lastChanged: unescapeHtml(LAST_CHANGED.exec(tag)?.[1] ?? ''),
    sourceUrl: unescapeHtml(sourceUrl),
  };
}

/** The twin's text: title, the facts line, then the source after its title, unchanged. */
function twinText(twin) {
  const text = readFileSync(twin.from, 'utf8');
  const title = TITLE_LINE.exec(text);
  if (!title) {
    throw new Error(`markdown-twins: ${twin.from} has no # title`);
  }
  const facts = [
    `Release: ${twin.facts.release}`,
    ...(twin.facts.lastChanged === '' ? [] : [`Last changed: ${twin.facts.lastChanged}`]),
    `Source: ${twin.facts.sourceUrl}`,
  ];
  const body = text.slice(title.index + title[0].length).replace(/^\s*\n/, '');
  return `${title[0]}\n\n> ${facts.join(' · ')}\n\n${body}`;
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

  // 2. Write each twin beside its page.
  for (const twin of twins) {
    writeFileSync(twin.to, twinText(twin));
  }

  // 3. llms.txt lists the twins.
  const links = rewriteLlms(new Set(twins.map((twin) => twin.url)));

  console.log(`markdown-twins: ${twins.length} Markdown twins, ${links} ${LLMS} links to them`);
}

main();

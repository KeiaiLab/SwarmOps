// /llms.txt (llmstxt.org): the site's name and tagline, a note on the links, then one section per
// docs sidebar group, with absolute links so the file reads the same wherever it is fetched from.
// scripts/markdown-twins.mjs then points every /docs link at the page's Markdown twin.
//
//   # SwarmOps
//   > <tagline>
//   ## For agents        docs/agents.md, first, then the docs home
//   ## Get started ...   one section per sidebar group, in sidebar order
//   ## Optional          one page per command or MCP tool, and the changelog
import { flattenTree, type Item } from 'fumadocs-core/page-tree';
import { SITE_NAME, SITE_TAGLINE, SITE_URL } from '@/lib/site';
import { repoPath, source } from '@/lib/source';

export const dynamic = 'force-static';
export const revalidate = false;

// The page written for programs: the first link of the file (site design, D47).
const AGENTS_PAGE = 'docs/agents.md';
const AGENTS_SECTION = 'For agents';

// llmstxt.org: links under this heading may be skipped when a shorter context is needed.
const OPTIONAL_SECTION = 'Optional';

// /docs/reference/commands/commit has 4 segments: one page per command or tool, generated.
const ITEM_PAGE_SEGMENTS = 4;
const CHANGELOG_PAGE = 'CHANGELOG.md';

const NOTE =
  'Each link below is a page of the documentation as Markdown; the same address without `.md` is ' +
  'the page as HTML.';

interface Link {
  repo: string;
  line: string;
}

/** One list line: a description never breaks the list. */
function listLine(title: string, url: string, description?: string): string {
  const text = (description ?? '').replace(/\s+/g, ' ').trim();
  const link = `- [${title}](${SITE_URL}${url})`;
  return text ? `${link}: ${text}` : link;
}

/** A page as one list line: `- [title](https://host/docs/...): description`. */
function linkOf(item: Item): Link | null {
  const page = source.getNodePage(item);
  if (!page) {
    return null;
  }

  return {
    repo: repoPath(page.data),
    line: listLine(page.data.title, page.url, page.data.description),
  };
}

/** Whether a page may be skipped: a generated per-item page or the changelog. */
function isOptional(item: Item, link: Link): boolean {
  const segments = item.url
    .split('/')
    .filter(Boolean);
  return segments.length >= ITEM_PAGE_SEGMENTS || link.repo === CHANGELOG_PAGE;
}

function section(title: string, lines: string[]): string {
  return `## ${title}\n\n${lines.join('\n')}`;
}

export function GET() {
  const sections: string[] = [];
  const optional: string[] = [];
  let agents: string | null = null;

  // 1. One section per sidebar group; the agents page and the optional pages are set aside.
  for (const node of source.getPageTree().children) {
    if (node.type !== 'folder') {
      continue;
    }
    const lines: string[] = [];
    for (const item of flattenTree(node.children)) {
      const link = linkOf(item);
      if (!link) {
        continue;
      }
      if (link.repo === AGENTS_PAGE) {
        agents = link.line;
        continue;
      }
      if (isOptional(item, link)) {
        optional.push(link.line);
        continue;
      }
      lines.push(link.line);
    }
    if (lines.length > 0) {
      sections.push(section(String(node.name), lines));
    }
  }

  // 2. The docs home (docs/README.md), which no sidebar group holds.
  const home = source.getPage([]);
  const homeLines = home ? [listLine(home.data.title, home.url, home.data.description)] : [];

  // 3. A docs tree without the agents page is a broken build, not a shorter file.
  if (!agents) {
    throw new Error(`llms.txt: no page for ${AGENTS_PAGE}`);
  }

  const body = [
    `# ${SITE_NAME}`,
    `> ${SITE_TAGLINE}`,
    NOTE,
    section(AGENTS_SECTION, [agents, ...homeLines]),
    ...sections,
    section(OPTIONAL_SECTION, optional),
  ].join('\n\n');

  return new Response(`${body}\n`, {
    headers: { 'Content-Type': 'text/plain; charset=utf-8' },
  });
}

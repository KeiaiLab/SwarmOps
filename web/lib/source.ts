// The docs and product sources. Docs pages keep their repository URLs (lib/docs-paths.ts) and are
// filed into the sidebar groups of docs-nav.json, one root folder (a sidebar tab) per group:
//
//   repository                      sidebar (virtual path)          URL
//   docs/guides/install.md     ->   get-started/install.md     ->   /docs/guides/install
//   docs/reference/cli.md      ->   reference/cli.md           ->   /docs/reference/cli
//   CHANGELOG.md               ->   project/CHANGELOG.md       ->   /docs/changelog
import { loader, type VirtualFile } from 'fumadocs-core/source';
import { toFumadocsSource } from 'fumadocs-mdx/runtime/server';
import { docs, product, roadmap } from '@/.source/server';
import { DOCS_ROUTE, NAV_GROUPS, docSlugs, groupPath } from './docs-paths';

type DocEntry = (typeof docs)[number];

const MARKDOWN_EXT = '.md';
const REST = '...';

/** The repository path of a docs entry (`docs/guides/install.md`). */
export function repoPath(entry: { info: { path: string } }): string {
  return entry.info.path;
}

/** `docs/guides/install.md` -> `install`, the name a meta file lists a page by. */
function fileStem(file: string): string {
  const base = file
    .split('/')
    .at(-1) ?? file;
  return base.slice(0, -MARKDOWN_EXT.length);
}

function docFiles(): VirtualFile<{ pageData: DocEntry; metaData: object }>[] {
  const files: VirtualFile<{ pageData: DocEntry; metaData: object }>[] = [];
  const filled = new Set<string>();

  // 1. Every page, filed under its group's folder; the docs index stays at the root.
  for (const entry of docs) {
    const file = repoPath(entry);
    const placed = groupPath(file);
    if (placed) {
      filled.add(placed.group);
    }
    files.push({
      type: 'page',
      path: placed ? `${placed.group}/${placed.path}` : 'index.md',
      absolutePath: entry.info.fullPath,
      slugs: docSlugs(file),
      data: entry,
    });
  }

  // 2. One root folder per group that has pages: named files first, the rest alphabetically.
  for (const group of NAV_GROUPS) {
    if (!filled.has(group.key)) {
      continue;
    }
    const named = group.files.map((file) => fileStem(file));
    files.push({
      type: 'meta',
      path: `${group.key}/meta.json`,
      data: {
        title: group.title,
        description: group.description,
        root: true,
        pages: [...named, REST],
      },
    });
  }

  // 3. The groups in docs-nav.json order.
  files.push({
    type: 'meta',
    path: 'meta.json',
    data: {
      pages: NAV_GROUPS
        .filter((g) => filled.has(g.key))
        .map((g) => g.key),
    },
  });
  return files;
}

export const source = loader({
  baseUrl: DOCS_ROUTE,
  source: { files: docFiles() },
});

// Product pages under /product, the roadmap at /roadmap.
export const productSource = loader({
  baseUrl: '/product',
  source: toFumadocsSource(product, []),
});

export const roadmapPage = roadmap[0];

// Where a repository Markdown file lives on the site. One module for the build (the remark link
// rewrite in source.config.ts) and the pages (lib/source.ts), so a link and the page it points to
// always agree.
//
//   docs/guides/install.md    -> /docs/guides/install
//   docs/reference/README.md  -> /docs/reference
//   docs/README.md            -> /docs
//   CHANGELOG.md              -> /docs/changelog
//   README.md, crates/...     -> https://github.com/KeiaiLab/SwarmOps/blob/stable/...
import nav from '../docs-nav.json';

export const REPO_URL = 'https://github.com/KeiaiLab/SwarmOps';
export const REPO_BRANCH = 'stable';
export const DOCS_ROUTE = '/docs';

const DOCS_DIR = 'docs/';
const CHANGELOG = 'CHANGELOG.md';
const CHANGELOG_SLUG = 'changelog';
const INDEX_FILE = 'README.md';
const MARKDOWN_EXT = '.md';
const ASSETS_DIR = 'docs/assets/';

// Public URL of docs/assets/ (copied into public/ before the build).
export const ASSETS_ROUTE = '/assets/docs/';

// Kept privately, or inputs another generator splices into pages: never pages themselves.
const EXCLUDED_DIRS = ['docs/superpowers/', 'docs/reference/notes/', 'docs/examples/'];

export interface NavGroup {
  key: string;
  title: string;
  description: string;
  files: string[];
  dirs: string[];
}

export const NAV_GROUPS: NavGroup[] = nav.groups;
const FALLBACK_GROUP = nav.fallback;

/** Whether a repository path is a docs page. */
export function isDocPage(repoPath: string): boolean {
  if (repoPath === CHANGELOG) {
    return true;
  }
  if (!repoPath.startsWith(DOCS_DIR) || !repoPath.endsWith(MARKDOWN_EXT)) {
    return false;
  }
  return !EXCLUDED_DIRS.some((dir) => repoPath.startsWith(dir));
}

/** The URL segments of a docs page under /docs. */
export function docSlugs(repoPath: string): string[] {
  if (repoPath === CHANGELOG) {
    return [CHANGELOG_SLUG];
  }

  // docs/a/b.md -> [a, b]; a README.md stands for its folder.
  const segments = repoPath
    .slice(DOCS_DIR.length, -MARKDOWN_EXT.length)
    .split('/');
  if (segments.at(-1) === INDEX_FILE.slice(0, -MARKDOWN_EXT.length)) {
    segments.pop();
  }
  return segments;
}

/** The site URL of a docs page. */
export function docUrl(repoPath: string): string {
  const slugs = docSlugs(repoPath);
  return slugs.length === 0 ? DOCS_ROUTE : `${DOCS_ROUTE}/${slugs.join('/')}`;
}

/** The GitHub page of any repository path. */
export function githubUrl(repoPath: string): string {
  return `${REPO_URL}/blob/${REPO_BRANCH}/${repoPath}`;
}

/** The sidebar group of a page, and its path inside that group's folder. */
export function groupPath(repoPath: string): { group: string; path: string } | null {
  // The docs index is the landing page, outside every group.
  if (docSlugs(repoPath).length === 0) {
    return null;
  }

  // 1. Named files keep their file name.
  const base = repoPath
    .split('/')
    .at(-1) ?? repoPath;
  for (const group of NAV_GROUPS) {
    if (group.files.includes(repoPath)) {
      return { group: group.key, path: base };
    }
  }

  // 2. A file under a group's directory keeps its path below that directory.
  for (const group of NAV_GROUPS) {
    const dir = group.dirs.find((d) => repoPath.startsWith(d));
    if (dir) {
      return { group: group.key, path: repoPath.slice(dir.length) };
    }
  }

  // 3. Anything else: the fallback group, by its path under docs/.
  return { group: FALLBACK_GROUP, path: repoPath.slice(DOCS_DIR.length) };
}

// POSIX-style join and normalise, for repository-relative paths ('..' above the root is dropped).
function joinPath(dir: string, relative: string): string {
  const parts = dir === '' ? [] : dir.split('/');
  for (const part of relative.split('/')) {
    if (part === '' || part === '.') {
      continue;
    }
    if (part === '..') {
      parts.pop();
      continue;
    }
    parts.push(part);
  }
  return parts.join('/');
}

const ABSOLUTE_URL = /^[a-z][a-z0-9+.-]*:|^\/\/|^#|^\//i;

/**
 * Rewrites a relative link of the Markdown file `fromPath` for the site.
 * `../operations.md#upgrades` from docs/guides/install.md -> `/docs/operations#upgrades`.
 * Absolute URLs, site paths and bare anchors come back unchanged.
 */
export function rewriteLink(fromPath: string, href: string): string {
  if (ABSOLUTE_URL.test(href)) {
    return href;
  }

  // 1. Split off the anchor and resolve the path against the file's directory.
  const hashAt = href.indexOf('#');
  const target = hashAt === -1 ? href : href.slice(0, hashAt);
  const hash = hashAt === -1 ? '' : href.slice(hashAt);
  const fromDir = fromPath
    .split('/')
    .slice(0, -1)
    .join('/');
  const repoPath = joinPath(fromDir, target);

  // 2. Pages on the site, assets copied beside them, and GitHub for the rest of the tree.
  if (isDocPage(repoPath)) {
    return docUrl(repoPath) + hash;
  }
  if (repoPath.startsWith(ASSETS_DIR)) {
    return ASSETS_ROUTE + repoPath.slice(ASSETS_DIR.length) + hash;
  }
  return githubUrl(repoPath) + hash;
}

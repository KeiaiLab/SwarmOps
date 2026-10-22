// Remark plugin: rewrites the relative links and images of repository Markdown to site URLs
// (lib/docs-paths.ts), so a page reads the same on GitHub and on the site.
import * as path from 'node:path';
import type { Root } from 'mdast';
import type { VFile } from 'vfile';
import { visit } from 'unist-util-visit';
import { rewriteLink } from './docs-paths';

export interface RepoLinksOptions {
  /** Absolute path of the repository root. */
  root: string;
}

export function remarkRepoLinks({ root }: RepoLinksOptions) {
  return (tree: Root, file: VFile) => {
    // Files outside the repository (none today) keep their links.
    const fromPath = path
      .relative(root, file.path)
      .split(path.sep)
      .join('/');
    if (fromPath.startsWith('..')) {
      return;
    }

    // Links, images and link definitions ([x]: ../a.md) alike.
    visit(tree, ['link', 'image', 'definition'], (node) => {
      if (!('url' in node) || typeof node.url !== 'string') {
        return;
      }
      node.url = rewriteLink(fromPath, node.url);
    });
  };
}

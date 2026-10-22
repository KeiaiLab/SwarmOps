// Content collections. The docs are the repository's own Markdown (../docs and ../CHANGELOG.md),
// never copied: the site renders what GitHub shows. Product pages are web/content/.
import * as path from 'node:path';
import { defineCollections, defineConfig } from 'fumadocs-mdx/config';
import { pageSchema } from 'fumadocs-core/source/schema';
import { z } from 'zod';
import { remarkRepoLinks } from './lib/remark-repo-links';
import { firstHeading, firstParagraph } from './lib/markdown-meta';

// The build runs in web/; the repository is its parent.
const REPO_ROOT = path.resolve('..');

// Repository docs: title and description from the text, front matter optional.
export const docs = defineCollections({
  type: 'doc',
  dir: '..',
  files: [
    'docs/**/*.md',
    'CHANGELOG.md',
    '!docs/superpowers/**',
    '!docs/reference/notes/**',
    '!docs/examples/**',
  ],
  schema: ({ source, path: file }) => {
    const title = firstHeading(source, path.basename(file, '.md'));
    return pageSchema.extend({
      title: z
        .string()
        .default(title),
      description: z
        .string()
        .default(firstParagraph(source) ?? title),
    });
  },
});

// Product pages and the roadmap: Markdown with title and description front matter.
export const product = defineCollections({
  type: 'doc',
  dir: 'content/product',
  files: ['**/*.md'],
  schema: pageSchema.extend({ description: z.string() }),
});

export const roadmap = defineCollections({
  type: 'doc',
  dir: 'content',
  files: ['roadmap.md'],
  schema: pageSchema.extend({ description: z.string() }),
});

export default defineConfig({
  mdxOptions: {
    // Links first, so the image plugin sees site URLs.
    remarkPlugins: (plugins) => [[remarkRepoLinks, { root: REPO_ROOT }], ...plugins],
    // Offline builds: no remote image is fetched, and an image without a size renders unsized.
    remarkImageOptions: { useImport: false, external: false, onError: 'ignore' },
    rehypeCodeOptions: {
      themes: { light: 'github-light', dark: 'github-dark' },
    },
  },
});

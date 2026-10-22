// Static export: every page is HTML in out/, served by Cloudflare Pages with no server runtime.
import * as path from 'node:path';
import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

/** @type {import('next').NextConfig} */
const config = {
  output: 'export',
  reactStrictMode: true,
  poweredByHeader: false,
  // `next dev` writes no AGENTS.md into web/.
  agentRules: false,
  // Images are static files; the export has no image optimiser.
  images: { unoptimized: true },
  // The docs are ../docs and ../CHANGELOG.md: the bundler's root is the repository.
  turbopack: { root: path.resolve('..') },
  outputFileTracingRoot: path.resolve('..'),
};

export default withMDX(config);

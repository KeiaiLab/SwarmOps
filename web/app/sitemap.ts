// /sitemap.xml: the home, product, roadmap and every docs page.
import type { MetadataRoute } from 'next';
import { SITE_URL } from '@/lib/site';
import { productSource, source } from '@/lib/source';

export const dynamic = 'force-static';

export default function sitemap(): MetadataRoute.Sitemap {
  const paths = [
    '/',
    '/roadmap',
    '/docs',
    ...productSource
      .getPages()
      .map((page) => page.url),
    ...source
      .getPages()
      .map((page) => page.url),
  ];

  // The docs index is a page too when docs/README.md exists: list it once.
  return [...new Set(paths)].map((path) => ({ url: `${SITE_URL}${path}` }));
}

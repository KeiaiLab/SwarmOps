// /sitemap.xml: the home, product, roadmap, /now and every docs page. /now's lastmod is the date
// of the commit built (STABLE.date), so it moves with every stable push and is never typed.
import type { MetadataRoute } from 'next';
import { SITE_URL } from '@/lib/site';
import { productSource, source } from '@/lib/source';
import { STABLE } from '@/lib/stable';

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

  // 1. The docs index is a page too when docs/README.md exists: list it once.
  const pages = [...new Set(paths)].map((path) => ({ url: `${SITE_URL}${path}` }));

  // 2. /now states the commit built: its date is the page's last change.
  return [...pages, { url: `${SITE_URL}/now`, lastModified: STABLE.date }];
}

// /robots.txt: every page may be indexed; the pages.dev and get.swarmops.cc hosts are kept out by
// X-Robots-Tag in public/_headers.
import type { MetadataRoute } from 'next';
import { SITE_URL } from '@/lib/site';

export const dynamic = 'force-static';

export default function robots(): MetadataRoute.Robots {
  return {
    rules: { userAgent: '*', allow: '/' },
    sitemap: `${SITE_URL}/sitemap.xml`,
  };
}

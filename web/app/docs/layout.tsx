// The docs shell: theme and search, header, sidebar (one tab per docs-nav.json group), search,
// and the site footer below the layout, the same as on product pages. The skip link comes first;
// the header Fumadocs shows under 768 px names the repository.
import type { ReactNode } from 'react';
import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import { DocsHeader } from '@/components/docs/docs-header';
import { Provider } from '@/components/provider';
import { SiteFooter } from '@/components/site-footer';
import { SkipLink } from '@/components/skip-link';
import { baseOptions } from '@/lib/layout.shared';
import { source } from '@/lib/source';

export default function Layout({ children }: { children: ReactNode }) {
  const base = baseOptions();

  return (
    <Provider>
      <SkipLink />
      <DocsLayout tree={source.getPageTree()} {...base} nav={{ ...base.nav, component: <DocsHeader /> }}>
        {children}
      </DocsLayout>
      <SiteFooter />
    </Provider>
  );
}

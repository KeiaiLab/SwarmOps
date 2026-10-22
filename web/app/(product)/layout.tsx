// The home, product and roadmap pages.
import type { ReactNode } from 'react';
import { SiteShell } from '@/components/site-shell';

export default function Layout({ children }: { children: ReactNode }) {
  return <SiteShell>{children}</SiteShell>;
}

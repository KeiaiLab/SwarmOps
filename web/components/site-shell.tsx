// The frame of every page outside /docs: theme, header, content, footer.
import type { ReactNode } from 'react';
import { SiteHeader } from '@/components/site-header';
import { ThemeProvider } from '@/components/theme-provider';
import { GITHUB_URL } from '@/lib/site';

export function SiteShell({ children }: { children: ReactNode }) {
  return (
    <ThemeProvider>
      <SiteHeader />
      {children}
      <footer className="mt-auto border-t">
        <div className="mx-auto flex w-full max-w-5xl flex-wrap gap-4 px-4 py-6 text-sm text-fd-muted-foreground">
          <span>SwarmOps · MIT licence</span>
          <a className="hover:text-fd-foreground" href={GITHUB_URL}>
            Source on GitHub
          </a>
        </div>
      </footer>
    </ThemeProvider>
  );
}

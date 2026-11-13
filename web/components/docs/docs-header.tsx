'use client';
// The docs header Fumadocs shows under 768 px (layouts/docs/slots/header), with the repository
// named in it; the sidebar, which holds the same link, is folded away at that width.
//
//   [mark] SwarmOps   GitHub          [search] [sidebar]
import { SidebarIcon } from 'lucide-react';
import { buttonVariants } from 'fumadocs-ui/components/ui/button';
import { useDocsLayout } from 'fumadocs-ui/layouts/docs';
import { GITHUB_URL } from '@/lib/site';

// Fumadocs' header classes, half spacing steps rounded down to whole ones.
const HEADER =
  '[grid-area:header] sticky top-(--fd-docs-row-1) z-30 flex items-center ps-4 pe-2 border-b transition-colors backdrop-blur-sm h-(--fd-header-height) md:hidden max-md:layout:[--fd-header-height:--spacing(14)] data-[transparent=false]:bg-fd-background/80';

export function DocsHeader() {
  const { isNavTransparent, slots } = useDocsLayout();

  return (
    <header id="nd-subnav" data-transparent={isNavTransparent} className={HEADER}>
      {slots.navTitle && <slots.navTitle className="inline-flex items-center gap-2 font-semibold" />}
      <nav aria-label="Repository" className="flex flex-1">
        <a href={GITHUB_URL} className="inline-flex min-h-11 items-center px-2 text-sm text-fd-muted-foreground hover:text-fd-foreground">
          GitHub
        </a>
      </nav>
      {slots.searchTrigger && <slots.searchTrigger.sm hideIfDisabled className="p-2" />}
      <slots.sidebar.trigger className={buttonVariants({ variant: 'ghost', size: 'icon-sm', className: 'p-2' })}>
        <SidebarIcon />
      </slots.sidebar.trigger>
    </header>
  );
}

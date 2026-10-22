// The product pages' header: logo, Product · Docs · Roadmap · Changelog, GitHub, theme. A server
// component with one small client island (the theme toggle), so these pages ship little script;
// the docs use Fumadocs' header, with the same links (lib/layout.shared.tsx).
import Link from 'next/link';
import { ThemeToggle } from '@/components/theme-toggle';
import { GITHUB_URL, NAV_LINKS, SITE_NAME } from '@/lib/site';

export function SiteHeader() {
  return (
    <header className="sticky top-0 z-40 border-b bg-fd-background/90 backdrop-blur">
      <div className="mx-auto flex h-14 w-full max-w-5xl items-center gap-4 px-4">
        <Link href="/" className="flex items-center gap-2 font-semibold">
          <img src="/logo.svg" alt="" width={24} height={24} />
          {SITE_NAME}
        </Link>
        <nav aria-label="Sections" className="flex flex-1 items-center gap-1 overflow-x-auto text-sm">
          {NAV_LINKS.map((link) => (
            <Link
              key={link.url}
              href={link.url}
              className="rounded-md px-2 py-1 text-fd-muted-foreground hover:bg-fd-accent hover:text-fd-accent-foreground"
            >
              {link.text}
            </Link>
          ))}
        </nav>
        <a
          href={GITHUB_URL}
          className="rounded-md px-2 py-1 text-sm text-fd-muted-foreground hover:bg-fd-accent hover:text-fd-accent-foreground"
        >
          GitHub
        </a>
        <ThemeToggle />
      </div>
    </header>
  );
}

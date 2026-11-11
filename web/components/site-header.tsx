// The product pages' header, a server component with no script of its own:
//
//   [mark] SwarmOps   Docs Product Roadmap Changelog GitHub     [Search /]  v0.11.0  [theme]
//
// One 56 px bar, --page-max wide. Under 1024 px the five links take a second 44 px row (one nav,
// no menu, no sideways scroll); under 640 px Search and the theme buttons show their icons only. The docs frame shows the same links (lib/layout.shared.tsx).
import Link from 'next/link';
import { Search } from 'lucide-react';
import { ThemeToggle } from '@/components/theme-toggle';
import { LATEST_RELEASE, releaseUrl } from '@/lib/release';
import { NAV_LINKS, SITE_NAME } from '@/lib/site';

// Product pages lead to the docs' search dialog; the site script follows it on `/` and Ctrl/Cmd+K.
const SEARCH_URL = '/docs?search=1';

// A header link or control: 44 px tall, quiet until hovered.
const ITEM =
  'inline-flex min-h-11 items-center rounded-md px-2 text-sm text-muted-foreground transition-colors duration-120 hover:text-foreground';

export function SiteHeader() {
  const { version } = LATEST_RELEASE;

  return (
    <>
      <a className="skip-link" href="#main">
        Skip to content
      </a>
      <header className="sticky top-0 z-40 border-b bg-background">
        <div className="page flex flex-wrap items-center gap-x-1 sm:gap-x-2">
          {/* 1. The mark and name, home. */}
          <Link href="/" className="inline-flex min-h-14 sm:mr-2 items-center gap-2 text-base font-semibold text-foreground">
            <img src="/logo.svg" alt="" width={24} height={24} />
            {SITE_NAME}
          </Link>

          {/* 2. The five sections, each once; a second row under 1024 px. */}
          <nav aria-label="Sections" className="order-last -mx-2 flex w-full border-t lg:order-none lg:mx-0 lg:w-auto lg:flex-1 lg:border-t-0">
            {NAV_LINKS.map((link) =>
              link.external ? (
                <a key={link.url} href={link.url} className={ITEM}>
                  {link.text}
                </a>
              ) : (
                <Link key={link.url} href={link.url} className={ITEM}>
                  {link.text}
                </Link>
              ),
            )}
          </nav>

          {/* 3. Search, the release and the theme, at the bar's end. */}
          <div className="ml-auto flex items-center sm:gap-1">
            <a href={SEARCH_URL} data-search className={`${ITEM} min-w-9 justify-center gap-2 max-sm:px-1`}>
              <Search aria-hidden="true" className="size-4" />
              <span className="max-sm:sr-only">Search</span>
              <kbd className="figure rounded-sm border border-input px-1 text-xs max-sm:hidden">/</kbd>
            </a>
            <a href={releaseUrl(LATEST_RELEASE)} className={`${ITEM} figure max-sm:px-1 max-sm:text-xs`} title="This release in the changelog">
              v{version}
            </a>
            <ThemeToggle />
          </div>
        </div>
      </header>
    </>
  );
}

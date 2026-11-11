// 404.html, which Cloudflare Pages serves for any path the export does not hold.
import Link from 'next/link';
import { SiteShell } from '@/components/site-shell';

const LINK = 'text-primary underline underline-offset-4';

export default function NotFound() {
  return (
    <SiteShell>
      <main id="main" className="page section flex w-full flex-col items-start gap-4">
        <p className="caption text-muted-foreground">404</p>
        <h1 className="font-serif text-3xl font-normal tracking-[-0.012em] text-foreground">Page not found</h1>

        {/* Two ways on: the docs home and search, both plain links. */}
        <p className="max-w-(--measure) text-base text-text">
          This address holds no page. Start at the{' '}
          <Link className={LINK} href="/docs">
            documentation home
          </Link>
          , or{' '}
          <a className={LINK} href="/docs?search=1">
            search the docs
          </a>
          .
        </p>
      </main>
    </SiteShell>
  );
}

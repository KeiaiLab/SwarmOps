// 404.html, which Cloudflare Pages serves for any path the export does not hold.
import Link from 'next/link';
import { SiteShell } from '@/components/site-shell';

export default function NotFound() {
  return (
    <SiteShell>
      <main id="main" className="mx-auto flex w-full max-w-3xl flex-col items-start gap-4 px-4 py-24">
        <h1 className="text-3xl font-bold">Page not found</h1>
        <p className="text-fd-muted-foreground">
          This address holds no page. The{' '}
          <Link className="text-fd-primary underline" href="/docs">
            documentation
          </Link>{' '}
          starts at /docs.
        </p>
      </main>
    </SiteShell>
  );
}

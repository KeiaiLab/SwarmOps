// /now: the commit the site was built from, the latest release, the cadence and the entries no
// release holds yet, all computed at build time (lib/stable.ts).
import type { Metadata } from 'next';
import { NowBody } from '@/components/now/now-facts';
import { STABLE } from '@/lib/stable';

const TITLE = 'On stable now';

export const metadata: Metadata = {
  title: TITLE,
  alternates: { canonical: '/now' },
};

export default function Page() {
  return (
    <main id="main" data-stable-commit={STABLE.commit} className="page pt-14 pb-24 max-sm:pt-8 max-sm:pb-16">
      {/* 1. The page's one h1 and what it states. */}
      <div className="mb-10">
        <h1 className="mb-4 font-serif text-3xl font-normal tracking-tight text-foreground">{TITLE}</h1>
        <p className="max-w-(--measure) text-lg text-muted-foreground">
          What this site was built from, and what {STABLE.ref} holds that no release does yet.
        </p>
      </div>

      {/* 2. The facts and the entries. */}
      <NowBody />
    </main>
  );
}

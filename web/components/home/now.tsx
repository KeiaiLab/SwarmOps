// Home's line on stable, between the first screen and the first commands:
//
//   On stable: 3a561c91 · 2026-11-12 · 81 commits since v0.11.0 · 9 unreleased changes ->
//
// `data-stable-commit` holds the full commit, so a check can compare the page with git.
import { commitUrl, unreleasedCount } from '@/components/now/now-facts';
import { STABLE } from '@/lib/stable';
import { Arrow, LINK } from './parts';

const NOW_URL = '/now';

export function Now() {
  return (
    <div className="page pb-10">
      <p
        data-stable-commit={STABLE.commit}
        className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-muted-foreground"
      >
        <span>
          On {STABLE.ref}:{' '}
          <a className={LINK + ' figure'} href={commitUrl(STABLE.commit)}>
            {STABLE.short}
          </a>
        </span>
        <span className="figure">{STABLE.date}</span>
        <span>
          {STABLE.since} commits since {STABLE.tag}
        </span>
        <a className={LINK + ' inline-flex items-center gap-1'} href={NOW_URL}>
          {unreleasedCount()} unreleased changes
          <Arrow />
        </a>
      </p>
    </div>
  );
}

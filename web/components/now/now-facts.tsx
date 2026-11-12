// /now's body: what the site was built from, the release cadence, and the Unreleased entries.
//
//   Commit            3a561c91 (GitHub), 2026-11-12
//   Branch            stable
//   Latest release    v0.11.0, 2026-11-09
//   Since the release 81 commits, 3 days
//   Release cadence   within 7 days or 200 commits; <verdict>
//   ## Not released yet (9)
//   ### Added ...     each entry with its code spans and links
//
// Server components only; every value comes from STABLE and LATEST_RELEASE at build time.
import type { ReactNode } from 'react';
import { H2, H3, LINK, PARA } from '@/components/home/parts';
import { REPO_URL } from '@/lib/docs-paths';
import { LATEST_RELEASE, releaseUrl } from '@/lib/release';
import { STABLE } from '@/lib/stable';
import { InlineText } from './inline-text';

const CHANGELOG_UNRELEASED_URL = '/docs/changelog#unreleased';

// The roadmap's status lines: the milestones on stable since the latest release.
const ROADMAP_URL = '/roadmap#on-stable';

/** The commit's page on GitHub. */
export function commitUrl(commit: string): string {
  return `${REPO_URL}/commit/${commit}`;
}

/** How many entries no release holds yet, over every group. */
export function unreleasedCount(): number {
  return STABLE.unreleased.reduce((sum, group) => sum + group.entries.length, 0);
}

function Facts() {
  const { cadence } = STABLE;
  const facts: { term: string; detail: ReactNode }[] = [
    {
      term: 'Commit',
      detail: (
        <>
          <a className={LINK + ' figure'} href={commitUrl(STABLE.commit)}>
            {STABLE.short}
          </a>
          , <span className="figure">{STABLE.date}</span>
        </>
      ),
    },
    { term: 'Branch', detail: <>{STABLE.ref}</> },
    {
      term: 'Latest release',
      detail: (
        <>
          <a className={LINK + ' figure'} href={releaseUrl(LATEST_RELEASE)}>
            v{LATEST_RELEASE.version}
          </a>
          , <span className="figure">{LATEST_RELEASE.date}</span>
        </>
      ),
    },
    {
      term: 'Since the release',
      detail: (
        <>
          {STABLE.since} commits, {STABLE.daysAfterTag} days
        </>
      ),
    },
    {
      term: 'Release cadence',
      detail: (
        <>
          A release follows the last within {cadence.maxDays} days or {cadence.maxCommits} commits.{' '}
          {cadence.over ? 'Over: ' : 'Within: '}
          {cadence.reason}.
        </>
      ),
    },
  ];

  return (
    <dl className="max-w-(--measure) border-t-2 border-rule-strong">
      {facts.map((fact) => (
        <div key={fact.term} className="border-b border-border py-3">
          <dt className="text-sm font-semibold text-muted-foreground">{fact.term}</dt>
          <dd className="mt-1 text-base text-(--text)">{fact.detail}</dd>
        </div>
      ))}
    </dl>
  );
}

function Unreleased() {
  return (
    <section aria-labelledby="unreleased-title" className="mt-14">
      {/* 1. The entries as CHANGELOG.md groups them, in file order. */}
      <h2 id="unreleased-title" className={H2}>
        Not released yet ({unreleasedCount()})
      </h2>
      {STABLE.unreleased.map((group) => (
        <div key={group.group} className="mt-8">
          <h3 className={H3}>{group.group}</h3>
          <ul className={PARA + ' mt-3 list-disc pl-5'}>
            {group.entries.map((entry) => (
              <li key={entry} className="mt-2">
                <InlineText text={entry} />
              </li>
            ))}
          </ul>
        </div>
      ))}

      {/* 2. The full text and the milestone lines live on their own pages. */}
      <p className={PARA + ' mt-10 flex flex-wrap gap-x-6'}>
        <a className={LINK} href={CHANGELOG_UNRELEASED_URL}>
          The full text in the changelog
        </a>
        <a className={LINK} href={ROADMAP_URL}>
          Milestones on the roadmap
        </a>
      </p>
    </section>
  );
}

/** /now's facts and entries, under the page's h1. */
export function NowBody() {
  return (
    <>
      <Facts />
      <Unreleased />
    </>
  );
}

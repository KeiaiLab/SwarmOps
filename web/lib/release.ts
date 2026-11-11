// The latest release, read from CHANGELOG.md when the site is built, so the pages never carry a
// version by hand. The top `## [x.y.z] - YYYY-MM-DD` section is the latest release; Unreleased
// is skipped.
import { readFileSync } from 'node:fs';
import * as path from 'node:path';

/** A released version and its date, as the changelog heads its section. */
export interface Release {
  /** `0.7.0`, without the tag's `v`. */
  version: string;
  /** `2026-10-29`. */
  date: string;
  /** Below 1.0: a minor release may still break a command or its output. */
  preRelease: boolean;
}

// The build runs in web/; the changelog is at the repository root.
const CHANGELOG = path.resolve('..', 'CHANGELOG.md');

// `## [0.7.0] - 2026-10-29`, Keep a Changelog's heading of a released section.
const RELEASED_HEADING = /^## \[(\d+\.\d+\.\d+)\] - (\d{4}-\d{2}-\d{2})\s*$/m;
const PRE_1_0 = '0.';

function readLatest(): Release {
  // 1. The first released heading; none fails the build rather than show a stale version.
  const text = readFileSync(CHANGELOG, 'utf8');
  const match = RELEASED_HEADING.exec(text);
  if (!match) {
    throw new Error(`${CHANGELOG}: no released section (## [x.y.z] - YYYY-MM-DD)`);
  }

  // 2. Version and date as written.
  const [, version, date] = match;
  return { version, date, preRelease: version.startsWith(PRE_1_0) };
}

/** The top released section of CHANGELOG.md. */
export const LATEST_RELEASE: Release = readLatest();

// GitHub's heading slug: lower case, punctuation but `-` dropped, each space a `-`.
// `[0.11.0] - 2026-11-09` -> `0110---2026-11-09`.
const SLUG_DROP = /[^\p{L}\p{N}\s_-]/gu;

/** The address of a release's entry in the changelog page. */
export function releaseUrl(release: Release): string {
  const heading = `[${release.version}] - ${release.date}`;
  const anchor = heading
    .toLowerCase()
    .replace(SLUG_DROP, '')
    .replace(/\s/g, '-');
  return `/docs/changelog#${anchor}`;
}

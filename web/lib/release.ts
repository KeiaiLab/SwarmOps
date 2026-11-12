// The latest release, computed when the site is built, so the pages never carry a version by
// hand: the newest `v*` tag reachable from the commit built, dated by its CHANGELOG.md section.
//
//   git describe --tags --abbrev=0 --match 'v[0-9]*' HEAD   -> v0.11.0
//   ## [0.11.0] - 2026-11-09                                -> date 2026-11-09
//
// Between a release commit and its tag the top section is not released yet; it counts as
// Unreleased (UNRELEASED) until the tag exists.
import { readFileSync } from 'node:fs';
import * as path from 'node:path';

import { parseUnreleased, releaseSection, type UnreleasedGroup } from './facts';
import { git, requireHistory } from './git';

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

// `v0.11.0`, the tags release.yml publishes.
const RELEASE_TAG = /^v(\d+\.\d+\.\d+)$/;
const TAG_GLOB = 'v[0-9]*';
const PRE_1_0 = '0.';

/** The version (`0.11.0`) of the newest release tag reachable from HEAD; none fails the build. */
function latestTag(): string {
  // 1. Tags need the full history; a shallow clone fails here rather than find none.
  requireHistory();

  // 2. The newest reachable tag, which must be a plain release version.
  let tag: string;
  try {
    tag = git(['describe', '--tags', '--abbrev=0', '--match', TAG_GLOB, 'HEAD']);
  } catch {
    throw new Error(`release: no tag matching ${TAG_GLOB} reachable from HEAD (fetch the tags)`);
  }
  const match = RELEASE_TAG.exec(tag);
  if (!match) {
    throw new Error(`release: newest tag ${tag} is not v<x.y.z>`);
  }
  return match[1];
}

const TEXT = readFileSync(CHANGELOG, 'utf8');

function readLatest(): Release {
  // The tag gives the version; its CHANGELOG section gives the date, or the build fails.
  const { version, date } = releaseSection(TEXT, latestTag());
  return { version, date, preRelease: version.startsWith(PRE_1_0) };
}

/** The newest release tag reachable from the commit built, dated by its CHANGELOG section. */
export const LATEST_RELEASE: Release = readLatest();

/** What no release tag holds yet: Unreleased plus any section above the tag's, grouped. */
export const UNRELEASED: UnreleasedGroup[] = parseUnreleased(TEXT, LATEST_RELEASE.version);

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

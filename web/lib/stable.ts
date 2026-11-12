// What the commit built is, computed from git when the site is built (the freshness design §2.2):
//
//   STABLE = { commit: 3a561c91…, short: 3a561c91, date: 2026-11-12, ref: stable,
//              tag: v0.11.0, tagDate: 2026-11-09, since: 81, daysAfterTag: 3,
//              unreleased: [{ group: 'Added', entries: [...] }, ...],
//              cadence: { maxDays: 7, maxCommits: 200, verdict: 'within', over: false, ... } }
//
// Dates are UTC days, so a build anywhere gives the same page. No build time: two builds of one
// commit state the same facts.
import { readFileSync } from 'node:fs';
import * as path from 'node:path';

import { cadence, type Cadence, type CadenceLimits, type UnreleasedGroup } from './facts';
import { git, requireHistory } from './git';
import { LATEST_RELEASE, UNRELEASED } from './release';

/** The commit built and where it stands against the latest release. */
export interface Stable {
  /** 40 hex. */
  commit: string;
  /** The first 8 hex of `commit`. */
  short: string;
  /** Committer date, UTC, `2026-11-12`. */
  date: string;
  /** `stable`, or the branch a preview build was made from. */
  ref: string;
  /** `v0.11.0`. */
  tag: string;
  /** The tag commit's committer date, UTC. */
  tagDate: string;
  /** Commits from the tag to the commit built. */
  since: number;
  /** Days from `tagDate` to `date`. */
  daysAfterTag: number;
  unreleased: UnreleasedGroup[];
  cadence: CadenceLimits & Cadence;
}

/** A file changed after the tag: its last-change commit, short, and that commit's UTC date. */
export interface FileAhead {
  short: string;
  date: string;
}

// The build runs in web/; the limits are shared with the workflows in .github/freshness.json.
const FRESHNESS = path.resolve('..', '.github', 'freshness.json');

const SHORT_LEN = 8;
const DAY_MS = 86_400_000;

// `TZ=UTC git log -1 --date=format-local:%Y-%m-%d --format=%cd`: the same day on every host.
const UTC = { TZ: 'UTC' };

/** `rev`'s committer date, or that of `file`'s last change up to `rev`, as a UTC day (`2026-11-12`). */
function utcDate(rev: string, file?: string): string {
  const args = ['log', '-1', '--date=format-local:%Y-%m-%d', '--format=%cd', rev];
  if (file !== undefined) {
    args.push('--', file);
  }
  return git(args, UTC);
}

/** The cadence limits from .github/freshness.json; a missing or non-numeric limit fails the build. */
function readLimits(): CadenceLimits {
  const config = JSON.parse(readFileSync(FRESHNESS, 'utf8')) as Record<string, unknown>;
  const maxDays = config.release_max_days;
  const maxCommits = config.release_max_commits;
  if (typeof maxDays !== 'number' || typeof maxCommits !== 'number') {
    throw new Error(`${FRESHNESS}: release_max_days and release_max_commits must be numbers`);
  }
  return { maxDays, maxCommits };
}

/** The branch built: CI's GITHUB_REF_NAME, else the checked-out branch. */
function readRef(): string {
  const fromCi = process.env.GITHUB_REF_NAME;
  if (fromCi) {
    return fromCi;
  }
  return git(['rev-parse', '--abbrev-ref', 'HEAD']);
}

/** Whole days from UTC day `from` to UTC day `to`. */
function daysBetween(from: string, to: string): number {
  return Math.round((Date.parse(`${to}T00:00:00Z`) - Date.parse(`${from}T00:00:00Z`)) / DAY_MS);
}

function readStable(): Stable {
  // 1. The commit built; LATEST_RELEASE already failed the build on a shallow clone or no tag.
  requireHistory();
  const commit = git(['rev-parse', 'HEAD']);
  const tag = `v${LATEST_RELEASE.version}`;

  // 2. Dates as UTC days and the distance from the tag.
  const date = utcDate('HEAD');
  const tagDate = utcDate(tag);
  const since = Number(git(['rev-list', '--count', `${tag}..HEAD`]));
  const daysAfterTag = daysBetween(tagDate, date);

  // 3. The cadence as of the commit built.
  const limits = readLimits();
  return {
    commit,
    short: commit.slice(0, SHORT_LEN),
    date,
    ref: readRef(),
    tag,
    tagDate,
    since,
    daysAfterTag,
    unreleased: UNRELEASED,
    cadence: { ...limits, ...cadence(daysAfterTag, since, limits) },
  };
}

/** The commit built, its date and its distance from the latest release. */
export const STABLE: Stable = readStable();

/** The file's last change when it has commits after the tag, else null (the release describes it). */
export function fileAhead(repoPath: string): FileAhead | null {
  // 1. No commit on the file after the tag: the page describes the release.
  const ahead = Number(git(['rev-list', '--count', `${STABLE.tag}..HEAD`, '--', repoPath]));
  if (ahead === 0) {
    return null;
  }

  // 2. The file's last-change commit, not the commit built, so unchanged pages stay unchanged.
  const commit = git(['log', '-1', '--format=%H', 'HEAD', '--', repoPath]);
  return { short: commit.slice(0, SHORT_LEN), date: utcDate('HEAD', repoPath) };
}

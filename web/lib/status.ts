// The milestone status lines, read from docs/status/ when the site is built (the freshness
// design §4, correction H7):
//
//   docs/status/M20/M20-4.txt  ->  { milestone: 'M20', slice: 'M20-4', sentence: 'The upgrade ...',
//                                    commit: 3a561c91..., date: '2026-11-11' }
//
// The commit is the one that added the file (or a seed file's named commit); its UTC day is the
// date. A malformed file, an uncommitted one or a seed commit HEAD does not reach fails the build.
import { readdirSync, readFileSync } from 'node:fs';
import * as path from 'node:path';

import { git, requireHistory } from './git';
import { LATEST_RELEASE } from './release';
import { STATUS_ROOT, checkSeed, parseStatusPath, parseStatusText } from './status-line';

/** One landing of a milestone. */
export interface StatusLine {
  slice: string;
  sentence: string;
  /** 40 hex. */
  commit: string;
  /** UTC day of `commit`, `2026-11-11`. */
  date: string;
}

/** A milestone and its landings since the latest release, oldest first. */
export interface MilestoneStatus {
  milestone: string;
  lines: StatusLine[];
}

// The build runs in web/; the status files sit at the repository root.
const STATUS_DIR = path.resolve('..', STATUS_ROOT);

// `TZ=UTC ... --date=format-local:%Y-%m-%d`: the same day on every host.
const UTC = { TZ: 'UTC' };

/** git's exit status as a verdict, for the commands whose non-zero exit is an answer. */
function gitSays(args: string[]): boolean {
  try {
    git(args);
    return true;
  } catch {
    return false;
  }
}

/** Whether `commit` exists and HEAD reaches it. */
function reachable(commit: string): boolean {
  return gitSays(['cat-file', '-e', `${commit}^{commit}`]) && gitSays(['merge-base', '--is-ancestor', commit, 'HEAD']);
}

/** The commit that added `repoPath` (the newest add, should the file have come back) and its UTC day. */
function addedBy(repoPath: string): { commit: string; date: string } {
  const out = git(['log', '--diff-filter=A', '--date=format-local:%Y-%m-%d', '--format=%H %cd', 'HEAD', '--', repoPath], UTC);
  const first = out.split('\n')[0];
  if (!first) {
    throw new Error(`${repoPath}: not committed, so it has no date`);
  }
  const [commit, date] = first.split(' ');
  return { commit, date };
}

/** A seed commit's UTC day. */
function commitDate(commit: string): string {
  return git(['log', '-1', '--date=format-local:%Y-%m-%d', '--format=%cd', commit], UTC);
}

/** One status file as a line; any fault names the file and fails the build. */
function readLine(dir: string, file: string): { milestone: string; line: StatusLine } {
  const { milestone, slice } = parseStatusPath(dir, file);
  const repoPath = `${STATUS_ROOT}/${dir}/${file}`;
  try {
    // 1. The sentence and, for a seed file, its commit, which HEAD must reach.
    const { sentence, seed } = parseStatusText(readFileSync(path.join(STATUS_DIR, dir, file), 'utf8'));
    if (seed !== null) {
      checkSeed(seed, reachable);
      return { milestone, line: { slice, sentence, commit: seed, date: commitDate(seed) } };
    }

    // 2. Otherwise the commit that added the file.
    return { milestone, line: { slice, sentence, ...addedBy(repoPath) } };
  } catch (error) {
    throw new Error(`${repoPath}: ${(error as Error).message}`);
  }
}

/** Every directory and file under docs/status/, sorted, each one checked. */
function readAll(): Map<string, StatusLine[]> {
  const byMilestone = new Map<string, StatusLine[]>();
  for (const dir of readdirSync(STATUS_DIR).sort()) {
    for (const file of readdirSync(path.join(STATUS_DIR, dir)).sort()) {
      const { milestone, line } = readLine(dir, file);
      byMilestone.set(milestone, [...(byMilestone.get(milestone) ?? []), line]);
    }
  }
  return byMilestone;
}

function readStatus(): MilestoneStatus[] {
  // 1. Every file, checked whether or not it is shown.
  requireHistory();
  const tag = `v${LATEST_RELEASE.version}`;
  const all = readAll();

  // 2. Lines the tag does not contain, by date and then slice id.
  const since = (line: StatusLine) => !gitSays(['merge-base', '--is-ancestor', line.commit, tag]);
  const byDate = (a: StatusLine, b: StatusLine) =>
    a.date.localeCompare(b.date) || a.slice.localeCompare(b.slice, 'en', { numeric: true });

  // 3. Milestones in number order, those with nothing since the tag left out.
  const number = (milestone: string) => Number(milestone.slice(1));
  return [...all.entries()]
    .map(([milestone, lines]) => ({ milestone, lines: lines.filter(since).sort(byDate) }))
    .filter((status) => status.lines.length > 0)
    .sort((a, b) => number(a.milestone) - number(b.milestone));
}

/** Per milestone, what landed on stable since the latest release. */
export const MILESTONE_STATUS: MilestoneStatus[] = readStatus();

// Pure rules over CHANGELOG.md text and release figures, no imports, so node's test runner loads
// this file as it is (web/test/facts.test.ts):
//
//   ## [Unreleased]            -> unreleased: every entry here
//   ## [0.12.0] - 2026-11-12   -> unreleased too while the newest tag is v0.11.0
//   ## [0.11.0] - 2026-11-09   -> releaseSection(text, '0.11.0'); scanning stops here

/** One CHANGELOG group (`### Added`) and its entries, each a `- ` item on one line. */
export interface UnreleasedGroup {
  group: string;
  entries: string[];
}

/** A released section's heading: `## [0.11.0] - 2026-11-09`. */
export interface ReleaseHeading {
  version: string;
  date: string;
}

/** Where the release cadence stands against its limits. */
export const CadenceVerdict = {
  Within: 'within',
  OverDays: 'over-days',
  OverCommits: 'over-commits',
} as const;
export type CadenceVerdict = (typeof CadenceVerdict)[keyof typeof CadenceVerdict];

/** The cadence rule: a release within `maxDays` days or `maxCommits` commits. */
export interface CadenceLimits {
  maxDays: number;
  maxCommits: number;
}

export interface Cadence {
  verdict: CadenceVerdict;
  over: boolean;
  reason: string;
}

const UNRELEASED_HEADING = '## [Unreleased]';
const SECTION = /^## /;
const GROUP = /^### (.+?)\s*$/;
const ENTRY = /^- (.*)$/;
const CONTINUATION = /^\s+\S/;

/** The heading line of `version`'s released section, e.g. `## [0.11.0] - 2026-11-09`. */
function releaseHeading(version: string): RegExp {
  const escaped = version.replace(/\./g, '\\.');
  return new RegExp(`^## \\[${escaped}\\] - (\\d{4}-\\d{2}-\\d{2})\\s*$`);
}

function noSection(version: string): Error {
  return new Error(`CHANGELOG.md: no \`## [${version}] - YYYY-MM-DD\` section for the tag v${version}`);
}

/** The released section of `version`, wherever it sits; throws when the changelog has none. */
export function releaseSection(changelog: string, version: string): ReleaseHeading {
  const heading = releaseHeading(version);
  for (const line of changelog.split('\n')) {
    const match = heading.exec(line);
    if (match) {
      return { version, date: match[1] };
    }
  }
  throw noSection(version);
}

/**
 * What no tag holds yet: the Unreleased entries plus every released section above the tag's
 * section, grouped by `###` heading in file order. Throws without `## [Unreleased]` or without
 * the tag's section below it.
 */
export function parseUnreleased(changelog: string, version: string): UnreleasedGroup[] {
  const lines = changelog.split('\n');

  // 1. Start at the Unreleased heading; without it the build cannot say what is new.
  const start = lines.findIndex((line) => line.trimEnd() === UNRELEASED_HEADING);
  if (start < 0) {
    throw new Error(`CHANGELOG.md: no \`${UNRELEASED_HEADING}\` section`);
  }

  // 2. Walk down to the tag's section; groups of one name merge, first appearance first.
  const tagHeading = releaseHeading(version);
  const groups = new Map<string, string[]>();
  let group: string[] | null = null;
  let entry: string[] = [];
  const close = () => {
    if (entry.length > 0 && group) {
      group.push(entry.join(' '));
    }
    entry = [];
  };
  for (const line of lines.slice(start + 1)) {
    // 2a. The tag's section ends the scan.
    if (tagHeading.test(line)) {
      close();
      return [...groups].map(([name, entries]) => ({ group: name, entries }));
    }

    // 2b. A section heading (a release above the tag) resets the group until its first `###`.
    if (SECTION.test(line)) {
      close();
      group = null;
      continue;
    }

    // 2c. A group heading opens or reopens its group.
    const groupMatch = GROUP.exec(line);
    if (groupMatch) {
      close();
      const name = groupMatch[1];
      group = groups.get(name) ?? [];
      groups.set(name, group);
      continue;
    }

    // 2d. An entry, or a continuation line of the open entry (`  wrapped text`).
    const entryMatch = ENTRY.exec(line);
    if (entryMatch && !group) {
      throw new Error(`CHANGELOG.md: entry outside a \`###\` group: ${line}`);
    }
    if (entryMatch) {
      close();
      entry = [entryMatch[1].trim()];
      continue;
    }
    if (entry.length > 0 && CONTINUATION.test(line)) {
      entry.push(line.trim());
      continue;
    }

    // 2e. Blank lines keep the entry open; prose (a section summary) closes it.
    if (line.trim() !== '') {
      close();
    }
  }
  throw noSection(version);
}

/** The cadence verdict for `days` and `commits` since the newest tag; at a limit is within. */
export function cadence(days: number, commits: number, limits: CadenceLimits): Cadence {
  if (days > limits.maxDays) {
    return {
      verdict: CadenceVerdict.OverDays,
      over: true,
      reason: `${days} days since the release; the limit is ${limits.maxDays}`,
    };
  }
  if (commits > limits.maxCommits) {
    return {
      verdict: CadenceVerdict.OverCommits,
      over: true,
      reason: `${commits} commits since the release; the limit is ${limits.maxCommits}`,
    };
  }
  return {
    verdict: CadenceVerdict.Within,
    over: false,
    reason: `${days} days, ${commits} commits since the release; within ${limits.maxDays} days or ${limits.maxCommits} commits`,
  };
}

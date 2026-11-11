// The goals table of docs/goals.md, read when the site is built, so the pages never carry a
// measured figure by hand. Each row keeps its cells as Markdown and counts the verdicts its
// Measured cell records; no word for the whole row is derived.
//
//   docs/goals.md  "## Goals"  | # | Goal | ... | Measured | Milestone |
//        |                                   |
//        v                                   v  each `perf.md, "Title"` citation
//   GoalRow { id, goal, target, measured, met, missed, open, ledger: /docs/perf#<heading id> }
//
// A table the parser cannot read stops the build and names the row, rather than show a stale or
// a wrong figure.
import { readFileSync } from 'node:fs';
import * as path from 'node:path';

/** One goal: its cells as written, its verdict counts and the ledger sections it cites. */
export interface GoalRow {
  id: string;
  goal: string;
  target: string;
  measured: string;
  milestone: string;
  met: number;
  missed: number;
  open: number;
  ledger: { title: string; url: string }[];
}

// The build runs in web/; the documents are in the repository's docs/.
const GOALS_FILE = path.resolve('..', 'docs', 'goals.md');
const PERF_FILE = path.resolve('..', 'docs', 'perf.md');
const PERF_URL = '/docs/perf';

const TABLE_HEADING = '## Goals';
const COLUMN = { id: '#', goal: 'Goal', target: 'Target', measured: 'Measured', milestone: 'Milestone' } as const;

// The verdicts a Measured cell records: `(met`, `(missed`, and what has no figure yet.
const MET = '(met';
const MISSED = '(missed';
const OPEN = /not (yet )?measured|not built/gi;

// `perf.md, "A", "B" and "C"`: the citation and its quoted titles.
const CITATION = /perf\.md,\s*((?:"[^"]+"(?:\s*(?:,|and)\s*)?)+)/g;
const QUOTED = /"([^"]+)"/g;

// A cited title may be a heading's opening words: `M17-3` for `M17-3: an import in short steps`.
const PREFIX_ENDS = [':', ' —', ' ('];

/** The goals shown on the home page, in this order. */
export const HOME_GOALS = ['G1', 'G2', 'G4', 'G6', 'G8', 'G22'];

/** GitHub's heading id: lower case, punctuation and symbols dropped, spaces to hyphens. */
function slug(text: string): string {
  return text
    .toLowerCase()
    .replace(/[^\p{L}\p{M}\p{N}\p{Pc} -]/gu, '')
    .replace(/ /g, '-');
}

/** A heading's text as the page renders it: Markdown marks dropped, a link's text kept. */
function plain(markdown: string): string {
  return markdown
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/`|\*\*?/g, '');
}

/** Every heading of a Markdown file, outside code fences, with the id the docs page gives it. */
function headings(markdown: string): { text: string; id: string }[] {
  const found: { text: string; id: string }[] = [];
  const seen = new Map<string, number>();
  let fenced = false;

  for (const line of markdown.split('\n')) {
    // 1. Code fences hold `#` lines that are no headings.
    if (/^\s*(```|~~~)/.test(line)) {
      fenced = !fenced;
      continue;
    }
    const match = /^#{1,6}\s+(.*?)\s*#*\s*$/.exec(line);
    if (fenced || !match) {
      continue;
    }

    // 2. A repeated id gets `-1`, `-2`, ... as the docs pages number it.
    const base = slug(plain(match[1]));
    let id = base;
    while (seen.has(id)) {
      const next = (seen.get(base) ?? 0) + 1;
      seen.set(base, next);
      id = `${base}-${next}`;
    }
    seen.set(id, 0);
    found.push({ text: match[1], id });
  }
  return found;
}

/** The ledger heading a goal cites: equal to the title, else the one heading it opens. */
function ledgerLink(goal: string, title: string, perf: { text: string; id: string }[]) {
  // 1. A heading equal to the title, as written or as rendered.
  const equal = perf.filter((h) => h.text === title || plain(h.text) === plain(title));
  if (equal.length === 1) {
    return { title, url: `${PERF_URL}#${equal[0].id}` };
  }
  if (equal.length > 1) {
    throw new Error(`${GOALS_FILE}: ${goal} cites perf.md "${title}", which ${equal.length} headings equal`);
  }

  // 2. The single heading that starts with the title and a separator.
  const opened = perf.filter((h) => PREFIX_ENDS.some((end) => h.text.startsWith(title + end)));
  if (opened.length !== 1) {
    throw new Error(`${GOALS_FILE}: ${goal} cites perf.md "${title}", which ${opened.length} headings start with`);
  }
  return { title, url: `${PERF_URL}#${opened[0].id}` };
}

/** A table row's cells; a `|` inside a code span or escaped as `\|` stays in its cell. */
function cells(line: string): string[] {
  const out: string[] = [];
  let cell = '';
  let code = false;

  for (let i = 0; i < line.length; i += 1) {
    const ch = line[i];
    if (ch === '\\' && line[i + 1] === '|') {
      cell += '|';
      i += 1;
      continue;
    }
    if (ch === '`') {
      code = !code;
    }
    if (ch === '|' && !code) {
      out.push(cell.trim());
      cell = '';
      continue;
    }
    cell += ch;
  }

  // The leading and trailing pipes leave an empty first and last piece.
  out.push(cell.trim());
  return out.slice(1, -1);
}

function count(text: string, word: string): number {
  return text.split(word).length - 1;
}

/** The table under `## Goals`, one row per goal. */
function readGoals(): GoalRow[] {
  // 1. The section and its table; a missing one stops the build.
  const lines = readFileSync(GOALS_FILE, 'utf8').split('\n');
  const start = lines.findIndex((line) => line.trim() === TABLE_HEADING);
  if (start < 0) {
    throw new Error(`${GOALS_FILE}: no "${TABLE_HEADING}" section`);
  }
  const end = lines.findIndex((line, i) => i > start && line.startsWith('## '));
  const section = lines.slice(start + 1, end < 0 ? undefined : end);
  const first = section.findIndex((line) => line.startsWith('|'));
  if (first < 0) {
    throw new Error(`${GOALS_FILE}: no table under "${TABLE_HEADING}"`);
  }
  let last = first;
  while (last < section.length && section[last].startsWith('|')) {
    last += 1;
  }

  // 2. The columns by name, so a new column elsewhere does not move them.
  const head = cells(section[first]);
  const index = Object.fromEntries(
    Object.entries(COLUMN).map(([key, name]) => {
      const at = head.indexOf(name);
      if (at < 0) {
        throw new Error(`${GOALS_FILE}: the goals table has no "${name}" column`);
      }
      return [key, at];
    }),
  ) as Record<keyof typeof COLUMN, number>;

  // 3. Each row below the separator: every cell filled, counts and ledger links derived.
  const perf = headings(readFileSync(PERF_FILE, 'utf8'));
  return section.slice(first + 2, last).map((line, n) => {
    const row = cells(line);
    const id = row[index.id] || `row ${n + 1}`;
    for (const [key, at] of Object.entries(index)) {
      if (!row[at]) {
        throw new Error(`${GOALS_FILE}: goal ${id}: the "${COLUMN[key as keyof typeof COLUMN]}" cell is empty`);
      }
    }

    const measured = row[index.measured];
    const titles = [...measured.matchAll(CITATION)].flatMap((m) => [...m[1].matchAll(QUOTED)].map((q) => q[1]));
    const ledger = titles
      .map((title) => ledgerLink(id, title, perf))
      .filter((link, i, all) => all.findIndex((other) => other.url === link.url) === i);

    return {
      id,
      goal: row[index.goal],
      target: row[index.target],
      measured,
      milestone: row[index.milestone],
      met: count(measured, MET),
      missed: count(measured, MISSED),
      open: measured.match(OPEN)?.length ?? 0,
      ledger,
    };
  });
}

/** Every goal of docs/goals.md, in its order. */
export const GOALS: GoalRow[] = readGoals();

/** The home page's goals; a named id the table lacks stops the build. */
export const HOME_ROWS: GoalRow[] = HOME_GOALS.map((id) => {
  const row = GOALS.find((goal) => goal.id === id);
  if (!row) {
    throw new Error(`${GOALS_FILE}: HOME_GOALS names ${id}, which the goals table lacks`);
  }
  return row;
});

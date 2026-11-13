// The goals the home page shows, generated from docs/goals.md (web/lib/goals.ts). A row shows
// the verdicts its Measured cell records as counts, never one word for the whole row, then the
// cell itself, whole, and the ledger sections it cites. A figure's paragraph holds its source:
// the target links the goals table, the measured cell its ledger sections.
//
//   G1  Many writers, one store         | 50 × 1,000 commits: ... (met) ... not measured yet
//   Target: 10 to 50 processes; ...     | Ledger: v0.6.0 — 2026-10-28 · D2-8
//   ✓ 1 met   ○ 1 not measured          |
import type { ReactNode } from 'react';
import { HOME_ROWS, type GoalRow } from '@/lib/goals';
import { H2, InlineMarkdown, LINK, LINK_44, Section } from './parts';

const ALL_GOALS_URL = '/docs/goals#goals';
const LEDGER_URL = '/docs/perf';

type Kind = 'met' | 'missed' | 'open';

// One mark per verdict; the word beside it carries the meaning, the colour only repeats it.
const MARK: Record<Kind, ReactNode> = {
  met: <path d="m3 8.5 3 3 7-7" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />,
  missed: <path d="m4 4 8 8m0-8-8 8" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />,
  open: <circle cx="8" cy="8" r="5" fill="none" stroke="currentColor" strokeWidth="1.8" />,
};

const WORD: Record<Kind, string> = { met: 'met', missed: 'missed', open: 'not measured' };

const COLOUR: Record<Kind, string> = {
  met: 'text-verdict-met',
  missed: 'text-verdict-missed',
  open: 'text-verdict-open',
};

const NO_VERDICT = 'No verdict in the ledger';

/** The verdict counts of one cell, zeros left out; one size and weight for all of them. */
function Counts({ row }: { row: GoalRow }) {
  const counts = (['met', 'missed', 'open'] as const)
    .map((kind) => ({ kind, n: row[kind] }))
    .filter((c) => c.n > 0);

  if (counts.length === 0) {
    return <p className="figure text-sm text-muted-foreground">{NO_VERDICT}</p>;
  }
  return (
    <ul aria-label="Verdicts recorded in the cell" className="figure flex list-none flex-wrap gap-x-5 gap-y-1 p-0 text-sm">
      {counts.map(({ kind, n }) => (
        <li key={kind} className={'inline-flex items-center gap-2 ' + COLOUR[kind]}>
          <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" className="flex-none">
            {MARK[kind]}
          </svg>
          {n} {WORD[kind]}
        </li>
      ))}
    </ul>
  );
}

function Ledger({ row }: { row: GoalRow }) {
  if (row.ledger.length === 0) {
    return null;
  }
  return (
    <span className="mt-3 block text-muted-foreground">
      <span className="caption mr-2">Ledger</span>
      {row.ledger.map((link, i) => (
        <span key={link.url}>
          {i > 0 && ' · '}
          <a className={LINK} href={link.url}>
            {link.title}
          </a>
        </span>
      ))}
    </span>
  );
}

function GoalEntry({ row }: { row: GoalRow }) {
  return (
    <article className="grid grid-cols-1 gap-x-12 gap-y-4 border-t border-border pt-6 pb-8 lg:grid-cols-[minmax(0,2fr)_minmax(0,3fr)]">
      {/* 1. The goal, its target and the counts. */}
      <div>
        <h3 className="text-base font-semibold text-foreground">
          <span className="figure mr-3 text-muted-foreground">{row.id}</span>
          <InlineMarkdown text={row.goal} />
        </h3>
        <p className="mt-2 text-sm">
          <a className={LINK + ' caption mr-2'} href={ALL_GOALS_URL}>
            Target
          </a>
          <InlineMarkdown text={row.target} />
        </p>
        <div className="mt-3">
          <Counts row={row} />
        </div>
      </div>

      {/* 2. The measured cell as docs/goals.md writes it, and in the same paragraph its ledger sections. */}
      <div className="min-w-0">
        <p className="text-sm">
          <span className="caption mr-2 text-muted-foreground">Measured</span>
          <InlineMarkdown text={row.measured} />
          <Ledger row={row} />
        </p>
      </div>
    </article>
  );
}

export function Evidence() {
  return (
    <Section id="evidence" n={4} label="Goals and measurements">
      <h2 id="evidence-title" className={H2}>
        Goals, as last measured
      </h2>

      <div className="mt-10">
        {HOME_ROWS.map((row) => (
          <GoalEntry key={row.id} row={row} />
        ))}
      </div>

      <p className="flex flex-wrap gap-x-6 border-t border-border pt-4">
        <a className={LINK_44} href={ALL_GOALS_URL}>
          All goals
        </a>
        <a className={LINK_44} href={LEDGER_URL}>
          Performance ledger
        </a>
      </p>
    </Section>
  );
}

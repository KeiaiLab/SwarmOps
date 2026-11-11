// When SwarmOps fits and when it does not, and what it costs: README.md "SwarmOps and git",
// docs/goals.md's non-goals and docs/known-limitations.md.
import type { ReactNode } from 'react';
import { C, H2, LINK, Section } from './parts';

const COLUMNS: { title: string; items: ReactNode[] }[] = [
  {
    title: 'Use it when',
    items: [
      <>
        Many agents on one host commit into one repository at once and need machine-readable answers and a land that
        merges and retries by itself.
      </>,
      <>
        You keep git for review, CI and deployment: the store imports from git and exports to it.
      </>,
    ],
  },
  {
    title: 'Do not use it when',
    items: [
      <>You need git parity: user hooks, signing, submodules or LFS.</>,
      <>You need macOS or Windows: Linux only, for now.</>,
      <>
        Several hosts write into one store at once: today one host writes at a time.
      </>,
      <>
        The store would sit on NFS, SMB, FUSE or under <C>/mnt/</C>.
      </>,
    ],
  },
  {
    title: 'What it costs',
    items: [
      <>Throughput: writers take turns under one lock with full fsync.</>,
      <>Disk: objects are stored one file each, with no packs.</>,
    ],
  },
];

export function Fit() {
  return (
    <Section id="fit" n={5} label="Fit">
      <h2 id="fit-title" className={H2}>
        When to use it
      </h2>

      {/* Three ruled columns; one under the other at 390. */}
      <div className="mt-10 grid grid-cols-1 gap-x-12 gap-y-10 md:grid-cols-3">
        {COLUMNS.map((column) => (
          <div key={column.title} className="border-t-2 border-rule-strong pt-4">
            <h3 className="text-base font-semibold text-foreground">{column.title}</h3>
            <ul className="mt-2 list-none p-0">
              {column.items.map((item, i) => (
                <li key={i} className="border-b border-border py-3 text-sm">
                  {item}
                </li>
              ))}
            </ul>
          </div>
        ))}
      </div>

      {/* Where each statement is kept. */}
      <p className="mt-8 flex flex-wrap gap-x-6 gap-y-2 text-sm">
        <a className={LINK} href="/docs/goals#non-goals">
          Non-goals
        </a>
        <a className={LINK} href="/docs/known-limitations">
          Known limitations
        </a>
        <a className={LINK} href="/product/vs-git">
          SwarmOps and git
        </a>
      </p>
    </Section>
  );
}

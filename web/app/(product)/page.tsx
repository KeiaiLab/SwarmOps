// The home page, top to bottom (anchors in brackets):
//
//   [top]        status line, h1, benefit sentence, install line, facts, Quick start
//                the line on stable: its commit, date and unreleased changes, linking /now
//   [first-run]  swarmops --version, init, and the MCP registration command
//   [sample]     eight agents commit into one store at once
//   [features]   the eight core features, one guide each
//   [evidence]   goals generated from docs/goals.md
//   [fit]        use it when, do not use it when, what it costs
//   [start]      Quick start again
//
// Server-rendered, no client component: the release comes from CHANGELOG.md and the goals from
// docs/goals.md at build time.
import type { Metadata } from 'next';
import { Evidence } from '@/components/home/evidence';
import { Features } from '@/components/home/features';
import { FirstRun } from '@/components/home/first-run';
import { Fit } from '@/components/home/fit';
import { Hero } from '@/components/home/hero';
import { Now } from '@/components/home/now';
import { Sample } from '@/components/home/sample';
import { Start } from '@/components/home/start';

export const metadata: Metadata = { alternates: { canonical: '/' } };

export default function HomePage() {
  return (
    <main id="main" className="text-base text-(--text)">
      <Hero />
      <Now />
      <FirstRun />
      <Sample />
      <Features />
      <Evidence />
      <Fit />
      <Start />
    </main>
  );
}

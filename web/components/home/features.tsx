// README.md, "Core features": eight features, each at most 45 words and one guide.
import type { ReactNode } from 'react';
import { cn } from '@/lib/cn';
import { Arrow, C, H2, H3, LINK_44, Section } from './parts';

const FEATURES: { title: string; text: ReactNode; guide: string; url: string }[] = [
  {
    title: 'Land',
    text: (
      <>
        <C>land</C> merges a change three-way onto a ref&apos;s head without a checkout, and moves the ref in one
        transaction.
      </>
    ),
    guide: 'Commit and land',
    url: '/docs/guides/commit-and-land',
  },
  {
    title: 'Workspaces',
    text: (
      <>
        <C>so ws create</C> writes a changeset&apos;s tree into a directory of its own; <C>ws land --check</C>{' '}
        publishes the tree the check ran on.
      </>
    ),
    guide: 'Build and test a change',
    url: '/docs/operate/build-and-test',
  },
  {
    title: 'One JSON envelope',
    text: (
      <>
        Every command prints one JSON envelope on stdout: <C>ok</C>, <C>op</C>, <C>api</C>, then <C>result</C> or{' '}
        <C>error</C>, then <C>meta</C>.
      </>
    ),
    guide: 'Scripting the CLI',
    url: '/docs/develop/cli-scripting',
  },
  {
    title: 'An MCP server',
    text: (
      <>
        <C>swarmops mcp</C> serves the store to an agent over stdio: 46 tools, one per operation, each returning the
        CLI&apos;s envelope.
      </>
    ),
    guide: 'Claude Code over MCP',
    url: '/docs/guides/claude-code',
  },
  {
    title: 'Op log and undo',
    text: (
      <>
        <C>so op undo &lt;op&gt;</C> moves every ref that operation moved back, by compare-and-swap.
      </>
    ),
    guide: 'History and undo',
    url: '/docs/guides/history-and-undo',
  },
  {
    title: 'Git import and export',
    text: (
      <>
        <C>git import</C> brings a git history into a store; <C>git export</C> writes it to a git branch.
      </>
    ),
    guide: 'Git round trip',
    url: '/docs/guides/git-round-trip',
  },
  {
    title: 'GitHub as a remote',
    text: (
      <>
        <C>push</C> moves a branch by compare-and-swap, never by force; <C>sync</C> lands the store&apos;s
        unpublished changes on the remote&apos;s head.
      </>
    ),
    guide: 'GitHub as a remote',
    url: '/docs/guides/github-remote',
  },
  {
    title: 'Crash safety and verify',
    text: (
      <>
        Full fsync, no fast mode. <C>verify --deep</C> re-hashes every object and checks the derived values against
        them.
      </>
    ),
    guide: 'Verify and repair',
    url: '/docs/operate/verify-and-repair',
  },
];

export function Features() {
  return (
    <Section id="features" n={3} label="What it does">
      <h2 id="features-title" className={H2}>
        Core features
      </h2>

      {/* Two columns of ruled rows; one at 390. */}
      <div className="mt-10 grid grid-cols-1 gap-x-16 sm:grid-cols-2">
        {FEATURES.map((feature) => (
          <article key={feature.title} className="border-t border-border pt-6 pb-8">
            <h3 className={cn(H3, 'mb-2')}>{feature.title}</h3>
            <p className="mb-2">{feature.text}</p>
            <a className={cn(LINK_44, 'gap-2 text-sm')} href={feature.url}>
              {feature.guide}
              <Arrow />
            </a>
          </article>
        ))}
      </div>
    </Section>
  );
}

// Home's core features: eight, each at most 45 words and one guide.
import type { ReactNode } from 'react';
import { cn } from '@/lib/cn';
import { Arrow, C, H2, H3, LINK_44, Section } from './parts';

const FEATURES: { title: string; text: ReactNode; guide: string; url: string }[] = [
  {
    title: 'A landing queue',
    text: (
      <>
        <C>so queue enqueue</C> takes a change in; the queue checks waiting changes in parallel and lands each one that
        passed, in order. A red one is found by bisection and leaves with its reason.
      </>
    ),
    guide: 'Commit and land',
    url: '/docs/guides/commit-and-land',
  },
  {
    title: 'One check file',
    text: (
      <>
        <C>.swarmops/checks.toml</C> names a repository&apos;s checks once. <C>so check run</C> runs a stage of them the
        same way for a person, an agent or CI, and a red test comes back as a finding.
      </>
    ),
    guide: 'Parallel agents',
    url: '/docs/guides/parallel-agents',
  },
  {
    title: 'Land and workspaces',
    text: (
      <>
        <C>land</C> merges a change three-way onto a ref&apos;s head without a checkout; <C>so ws create</C> writes a
        tree to disk, and <C>ws land --check</C> publishes the tree the check ran on.
      </>
    ),
    guide: 'Build and test a change',
    url: '/docs/operate/build-and-test',
  },
  {
    title: 'Answers for programs and people',
    text: (
      <>
        A program reading <C>so</C> gets one JSON envelope with a stable <C>E_*</C> code; a person at a terminal gets
        short text. <C>swarmops mcp</C> serves every operation to an agent as an MCP tool.
      </>
    ),
    guide: 'Scripting the CLI',
    url: '/docs/develop/cli-scripting',
  },
  {
    title: 'Your own server',
    text: (
      <>
        <C>swarmops serve</C> serves stores to git and to <C>so</C> over ssh and HTTPS, under an owner as a forge names
        them: <C>host:acme/app.git</C>. Packages, an image and a Helm chart, with encrypted backups.
      </>
    ),
    guide: 'Serve over ssh',
    url: '/docs/operate/serve-over-ssh',
  },
  {
    title: 'GitHub as a remote',
    text: (
      <>
        <C>push</C> moves a branch by compare-and-swap, never by force; <C>sync</C> lands the store&apos;s
        unpublished changes on the remote&apos;s head. A release starts from a pushed <C>release/X.Y.Z</C> branch.
      </>
    ),
    guide: 'GitHub as a remote',
    url: '/docs/guides/github-remote',
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
    title: 'Crash safety and verify',
    text: (
      <>
        Full fsync, no fast mode. <C>verify --deep</C> re-hashes every object and checks the derived values against
        them; <C>git export</C> writes the history back to git, byte for byte.
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

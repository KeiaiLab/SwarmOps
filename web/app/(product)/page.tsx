// The home page. Every sentence comes from README.md (the tagline, the opening paragraph, "Why
// SwarmOps" and the install line) or CHANGELOG.md (the release).
import type { Metadata } from 'next';
import Link from 'next/link';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { GITHUB_URL } from '@/lib/site';

export const metadata: Metadata = { alternates: { canonical: '/' } };

const INSTALL = 'curl -fsSL https://get.swarmops.cc/install.sh | bash';

// README.md, "Why SwarmOps".
const REASONS = [
  {
    title: 'Many agents write at once.',
    body: 'Tens of agent processes commit into one repository on one host. SwarmOps gives them one store that each of them writes directly: object writes take no lock, metadata writes queue in the kernel for one lock, and refs move only by compare-and-swap.',
  },
  {
    title: 'Agents read answers, not prose.',
    body: 'Every command prints one JSON envelope: a stable E_* code, a fixed exit code, whether retrying can help, machine-readable details and a hint, often the next command to run.',
  },
  {
    title: 'Agents get killed mid-write.',
    body: 'Durability is full fsync, with no fast mode: a commit returns only once its journal frame is on disk, a crash leaves no stale lock, and the next writer repairs whatever a crash cut short.',
  },
];

export default function HomePage() {
  return (
    <main id="main" className="mx-auto w-full max-w-5xl px-4 py-16">
      {/* 1. Positioning, the install line, the way in. */}
      <section className="flex flex-col items-start gap-6">
        <Badge variant="outline">Pre-release · v0.1.0 · Linux · MIT</Badge>
        <h1 className="text-4xl font-bold tracking-tight sm:text-5xl">
          A git replacement for <span className="text-brand">coding-agent swarms</span>
        </h1>
        <p className="max-w-3xl text-lg text-fd-muted-foreground">
          Many agents commit into one embedded store at once, every command answers in JSON, and the
          work moves back to git. Written in Rust and fully embedded: one program, no external
          database, no daemon.
        </p>
        <pre className="w-full max-w-3xl overflow-x-auto rounded-lg border bg-fd-card px-4 py-3 text-sm">
          <code>{INSTALL}</code>
        </pre>
        <div className="flex flex-wrap gap-3">
          <Button asChild size="lg">
            <Link href="/docs">Read the docs</Link>
          </Button>
          <Button asChild size="lg" variant="outline">
            <Link href="/product">What it is</Link>
          </Button>
          <Button asChild size="lg" variant="ghost">
            <a href={GITHUB_URL}>GitHub</a>
          </Button>
        </div>
      </section>

      {/* 2. Why a swarm needs it. */}
      <section className="mt-20">
        <h2 className="mb-6 text-2xl font-semibold">Why SwarmOps</h2>
        <div className="grid gap-4 md:grid-cols-3">
          {REASONS.map((reason) => (
            <Card key={reason.title}>
              <CardHeader>
                <CardTitle>
                  <h3>{reason.title}</h3>
                </CardTitle>
              </CardHeader>
              <CardContent className="text-sm text-fd-muted-foreground">{reason.body}</CardContent>
            </Card>
          ))}
        </div>
      </section>
    </main>
  );
}

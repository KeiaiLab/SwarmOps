// The home page, as the readable-site design board's Home boards lay it out: the hero (status
// line, title, who it is for, the install line, the way in, the facts), the numbered "On this
// page" list, then six sections. The release comes from CHANGELOG.md at build time; every other
// sentence comes from README.md (the opening paragraphs, "Core features", "SwarmOps and git",
// "Roadmap"), the product overview ("The problem", "The goal"), docs/goals.md, CHANGELOG.md's
// 0.5.0 entry or docs/README.md. Server-rendered, no client component.
import type { Metadata } from 'next';
import { Fragment, type ReactNode } from 'react';
import { AtAGlance, SelectFigure } from '@/components/markdown-page';
import { cn } from '@/lib/cn';
import { LATEST_RELEASE } from '@/lib/release';
import { GITHUB_URL } from '@/lib/site';

export const metadata: Metadata = { alternates: { canonical: '/' } };

const INSTALL = 'curl -fsSL https://get.swarmops.cc/install.sh | bash';

// One content edge for every block, 1040 wide; 20 px margins at 390.
const WRAP = 'mx-auto w-full max-w-[1104px] px-8 max-sm:px-5';

const CAP = 'text-[13px]/5 font-semibold uppercase tracking-[0.08em] text-muted-foreground';
const H2 = 'font-serif text-[30px]/[38px] font-normal tracking-[-0.005em] text-foreground max-sm:text-[26px]/[34px]';
const H3 = 'text-[19px]/7 font-semibold text-foreground';
const PARA = 'max-w-[60ch]';
const LINK =
  'text-primary underline decoration-1 underline-offset-[0.22em] decoration-[color-mix(in_srgb,var(--primary)_55%,transparent)] hover:decoration-2 hover:decoration-primary';
const LINK_44 = cn(LINK, 'inline-flex min-h-11 items-center');

// The sections, in page order: their anchors, numbers and labels feed the "On this page" list.
const SECTIONS = [
  { id: 'see-it', label: 'See it' },
  { id: 'why', label: 'Why a swarm needs it' },
  { id: 'today', label: 'What it does today' },
  { id: 'stands', label: 'Where it stands' },
  { id: 'fit', label: 'Fit' },
  { id: 'read-on', label: 'Read on' },
] as const;

type SectionId = (typeof SECTIONS)[number]['id'];

/** Inline code outside the prose stylesheet: paper3, 2 px under its text, free to break. */
function C({ children }: { children: ReactNode }) {
  return (
    <code className="rounded-[4px] bg-muted px-[0.32em] py-[0.1em] font-mono text-[calc(1em-2px)] text-foreground [overflow-wrap:anywhere]">
      {children}
    </code>
  );
}

function Arrow() {
  return (
    <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">
      <path d="M2 7h10M8 3l4 4-4 4" />
    </svg>
  );
}

function Section({ id, children }: { id: SectionId; children: ReactNode }) {
  const index = SECTIONS.findIndex((s) => s.id === id);
  return (
    <section id={id} className="scroll-mt-14 border-t border-border pt-[72px] pb-20 max-sm:pt-12 max-sm:pb-14">
      <div className={WRAP}>
        <p className={cn(CAP, 'mb-3.5 flex gap-3')}>
          <span className="text-foreground tabular-nums">{String(index + 1).padStart(2, '0')}</span>
          {SECTIONS[index].label}
        </p>
        {children}
      </div>
    </section>
  );
}

// ---------------------------------------------------------------------------------------------
// The hero

function Hero() {
  const { version, date, preRelease } = LATEST_RELEASE;
  const status = [preRelease ? 'Pre-release' : null, `v${version}, ${date}`, 'Linux', 'MIT']
    .filter((part) => part !== null)
    .join(' · ');

  return (
    <section className={cn(WRAP, 'grid grid-cols-1 pt-16 pb-[72px] max-sm:pt-7 max-sm:pb-12 lg:grid-cols-[minmax(0,1fr)_300px] lg:gap-x-20')}>
      {/* 1. Status, title and subtitle, across both columns. */}
      <div className="mb-8 max-sm:mb-4 lg:col-span-2">
        <p className="inline-flex items-center gap-2.5 text-[15px]/6 text-muted-foreground max-sm:text-[14px]/[22px]">
          <span aria-hidden="true" className="size-2 flex-none rounded-full bg-ring" />
          {status}
        </p>
        <h1 className="mt-4 mb-3 max-w-[15em] font-serif text-[60px]/[68px] font-normal tracking-[-0.012em] text-foreground max-sm:mt-3 max-sm:mb-2 max-sm:text-[32px]/[40px]">
          A git replacement for <span className="whitespace-nowrap">coding-agent</span> swarms
        </h1>
        <p className="text-[22px]/8 text-muted-foreground max-sm:text-[18px]/[30px]">
          Many writers, one store, one JSON envelope per command.
        </p>
      </div>

      {/* 2. Who it is for, the install line and the way in; at 390 the install line comes first. */}
      <div className="flex flex-col items-start max-sm:items-stretch">
        <p className={cn(PARA, 'text-[20px]/8 max-sm:order-1 max-sm:mb-5 max-sm:text-[18px]/[30px]')}>
          SwarmOps is source control for a swarm of coding agents: 10 to 50 Claude Code or LangGraph processes on one
          host commit into one embedded store at once, and every command answers in one JSON envelope.
        </p>
        <p className={cn(PARA, 'mt-4 mb-8 text-[20px]/8 max-sm:order-5 max-sm:mt-6 max-sm:mb-0 max-sm:text-[18px]/[30px]')}>
          It is one Rust program, with no external database and no daemon, and it moves the work to and from git, so
          that CI and deployments keep running.
        </p>
        <div className="mb-3 w-full max-w-[640px] max-sm:order-2 max-sm:mb-4">
          <p className="mb-2 flex items-baseline justify-between text-[14px]/5 text-muted-foreground">
            <span className={CAP}>Install</span>
            <span className="max-sm:hidden">One click selects the line</span>
          </p>
          <code className="block rounded-lg border border-input bg-card px-[18px] py-4 font-mono text-[16px]/6 break-words whitespace-pre-wrap text-foreground [user-select:all] max-sm:px-3.5 max-sm:py-3 max-sm:text-[15px]">
            {INSTALL}
          </code>
        </div>
        <p className="mb-6 max-w-[640px] text-[14px]/6 text-muted-foreground max-sm:order-4 max-sm:mt-4 max-sm:mb-0">
          Puts <C>swarmops</C> and <C>so</C>, its short name, in <C>~/.local/bin</C>, checked against the release&apos;s{' '}
          <C>SHA256SUMS</C>. Run it again to update.
        </p>
        <div className="flex flex-wrap items-center gap-x-7 gap-y-2 max-sm:order-3 max-sm:grid max-sm:grid-cols-2 max-sm:gap-x-2 max-sm:gap-y-1">
          <a
            href="/docs/guides/quick-start"
            className="inline-flex min-h-12 items-center gap-2.5 rounded-lg bg-primary px-[22px] text-[16px] font-semibold text-primary-foreground hover:underline max-sm:col-span-2 max-sm:min-h-[52px] max-sm:justify-center"
          >
            Start the quick start
            <Arrow />
          </a>
          <a href="/product" className={cn(LINK_44, 'max-sm:justify-center')}>
            What it is
          </a>
          <a href={GITHUB_URL} className={cn(LINK_44, 'max-sm:justify-center')}>
            Source on GitHub
          </a>
        </div>
      </div>

      {/* 3. The facts, beside the text on wide screens. */}
      <AtAGlance className="max-lg:mt-10" />
    </section>
  );
}

function JumpList() {
  return (
    <nav aria-label="On this page" className="border-t border-border pt-5 pb-6 max-sm:pt-4 max-sm:pb-5">
      <div className={cn(WRAP, 'flex flex-wrap items-center gap-x-1 gap-y-2 max-sm:block')}>
        <p className={cn(CAP, 'mr-4 max-sm:mb-1')}>On this page</p>
        <ol className="flex list-none flex-wrap gap-x-1 p-0 max-sm:grid max-sm:grid-cols-1">
          {SECTIONS.map((section, i) => (
            <li key={section.id}>
              <a
                href={`#${section.id}`}
                className="inline-flex min-h-11 items-center gap-2 px-3 text-[15px] text-[var(--text)] underline decoration-[var(--input)] decoration-1 underline-offset-[0.22em] max-sm:min-h-12 max-sm:px-1"
              >
                <span className="text-[14px] text-muted-foreground tabular-nums">{String(i + 1).padStart(2, '0')}</span>
                {section.label}
              </a>
            </li>
          ))}
        </ol>
      </div>
    </nav>
  );
}

// ---------------------------------------------------------------------------------------------
// 01 See it: README.md, "Many agents commit into one store at once", with its real output.

type Token = 'str' | 'flag' | 'var' | 'com';

// The Tokens board's "Code colours", as source.config.ts gives them to Shiki; comments in muted.
const TOKEN_CLASS: Record<Token, string> = {
  str: 'text-[#116b3d] dark:text-[#7fd19b]',
  flag: 'text-[#5a3bd6] dark:text-[#b9a8ff]',
  var: 'text-[#9a4500] dark:text-[#f0a35a]',
  com: 'text-muted-foreground',
};

function T({ k, children }: { k: Token; children: ReactNode }) {
  return <span className={TOKEN_CLASS[k]}>{children}</span>;
}

const LANDED = [
  'landed 42df54c0… on main (onto 1a2861f1…, attempt 4)',
  'landed 8dff860c… on main (onto c1a3b584…, attempt 1)',
  'landed fb162384… on main (onto 80d99e24…, attempt 4)',
  'landed 0d8001c4… on main (onto 42df54c0…, attempt 5)',
  'landed c1a3b584… on main (onto 4a7bc757…, attempt 1)',
  'landed e0f1f8a6… on main (onto 0d8001c4…, attempt 5)',
  'landed 80d99e24… on main (onto 8dff860c…, attempt 3)',
  'landed 1a2861f1… on main (onto fb162384…, attempt 4)',
];

const ENTRIES = Array.from({ length: 8 }, (_, i) => `entries[${i}].name: ${i + 1}.txt`);

// Output wraps after , : { [ / instead of scrolling, as source.config.ts does for the docs.
const BREAK_AFTER = /([,:{[/])/;

function outputText(lines: string[]) {
  return (
    <samp className="code-out-text">
      {lines.map((line, i) => (
        <Fragment key={line}>
          {i > 0 && '\n'}
          {line
            .split(BREAK_AFTER)
            .filter((part) => part !== '')
            .map((part, j) =>
              BREAK_AFTER.test(part) ? (
                <Fragment key={j}>
                  {part}
                  <wbr />
                </Fragment>
              ) : (
                part
              ),
            )}
        </Fragment>
      ))}
    </samp>
  );
}

function SeeIt() {
  return (
    <Section id="see-it">
      <h2 className={cn(H2, 'mb-5')}>Many agents commit into one store at once</h2>
      <p className={PARA}>
        Each agent process writes the store directly. Here eight agents each commit a note and land it on <C>main</C>,
        all at once:
      </p>

      {/* The command and what it printed, one figure; the listing folds after its first 8 lines. */}
      <div className="code-ex mt-7">
        <SelectFigure label="Shell">
          <code>
            work() {'{'}  <T k="com"># one agent: commit a note, then land it on main</T>
            {'\n'}  local me=agent/worker-<T k="var">$1</T> cs
            {'\n'}  printf{' '}
            <T k="str">{`'[{"path": "notes/%s.txt", "op": "put", "content": {"inline": "from worker %s\\\\n"}}]'`}</T>{' '}
            <T k="var">$1</T> <T k="var">$1</T> &gt; note-<T k="var">$1</T>.json
            {'\n'}  cs=<T k="var">$(</T>so <T k="flag">--as</T> <T k="var">$me</T> <T k="flag">--human</T> commit{' '}
            <T k="flag">-m</T> <T k="str">&quot;note $1&quot;</T> <T k="flag">--changes</T> note-<T k="var">$1</T>.json |
            sed <T k="flag">-n</T> <T k="str">&apos;s/^changeset: //p&apos;</T>)
            {'\n'}  so <T k="flag">--as</T> <T k="var">$me</T> <T k="flag">--human</T> land <T k="str">&quot;$cs&quot;</T>
            {'\n'}
            {'}'}
            {'\n'}for i in 1 2 3 4 5 6 7 8; do work <T k="var">$i</T> &gt; land-<T k="var">$i</T>.txt &amp; done; wait
            {'\n'}cat land-*.txt
            {'\n'}so <T k="flag">--human</T> ls main:notes | grep name
          </code>
        </SelectFigure>
        <figure className="code-out not-prose">
          <figcaption>
            <span className="code-label">Output</span>
          </figcaption>
          {outputText(LANDED)}
          <details className="code-fold">
            <summary>
              <span>Show {ENTRIES.length} more lines</span>
            </summary>
            {outputText(ENTRIES)}
          </details>
        </figure>
      </div>

      <p className={cn(PARA, 'mt-6')}>
        All eight landed. <C>attempt</C> is the try that succeeded: a land whose <C>main</C> moved under it merged again
        and retried by itself.{' '}
        <a className={LINK} href="/docs/guides/parallel-agents">
          Parallel agents
        </a>{' '}
        compares this with agents in git worktrees.
      </p>
    </Section>
  );
}

// ---------------------------------------------------------------------------------------------
// 02 Why: the product overview, "The problem" against "The goal", one need to one answer.

const PAIRS: { problem: string; problemText: ReactNode; answer: string; answerText: ReactNode }[] = [
  {
    problem: 'Many agents write at once.',
    problemText: (
      <>
        Tens of agent processes commit into one repository on one host. With git, each agent works in its own worktree
        and branch, and publishing is left to the agent: git&apos;s push refuses a moved branch and leaves the fetch,
        the merge and the retry to the pusher.
      </>
    ),
    answer: 'Publishing that merges by itself.',
    answerText: (
      <>
        <C>land</C> merges an agent&apos;s change three-way onto <C>main</C> and publishes it in one transaction,
        merging again and retrying when <C>main</C> moves; a conflict comes back as a JSON report, and nothing is
        recorded.
      </>
    ),
  },
  {
    problem: 'Locks outlive their holders.',
    problemText: (
      <>
        A git process killed with <C>SIGKILL</C> while it holds <C>index.lock</C> or a ref&apos;s <C>.lock</C> leaves
        the file behind, to be removed by hand.
      </>
    ),
    answer: 'One store, many writers.',
    answerText: (
      <>
        Object writes take no lock, metadata writes queue in the kernel for one lock, and refs move only by
        compare-and-swap. Every commit and ref move is a frame of one journal, so all of them have a single global
        order.
      </>
    ),
  },
  {
    problem: 'Agents read answers, not prose.',
    problemText: (
      <>
        Git reports errors as free text on stderr. An agent needs a stable code, whether retrying can help, and what to
        do next.
      </>
    ),
    answer: 'One JSON envelope per command.',
    answerText: (
      <>
        A stable <C>E_*</C> code, a fixed exit code, <C>retryable</C>, machine-readable <C>details</C> and a{' '}
        <C>hint</C>; <C>swarmops schema</C> describes every type, and <C>swarmops mcp</C> serves the operations to MCP
        clients such as Claude Code.
      </>
    ),
  },
  {
    problem: 'Agents get killed mid-write.',
    problemText: (
      <>
        Git&apos;s defaults (2.43) sync packs, pack metadata and the commit graph, not loose objects, refs or the index.
      </>
    ),
    answer: 'Full fsync, always.',
    answerText: (
      <>
        A commit returns only once its journal frame is on disk, a crash leaves no stale lock (the lock is an{' '}
        <C>flock</C>, which the kernel releases when its holder dies), and the next writer repairs whatever a crash cut
        short.
      </>
    ),
  },
];

// At 390 the column heads go and each half names its side above itself.
const PAIR_HALF =
  'max-sm:before:mb-1.5 max-sm:before:block max-sm:before:text-[13px]/5 max-sm:before:font-semibold max-sm:before:uppercase max-sm:before:tracking-[0.08em] max-sm:before:text-muted-foreground max-sm:before:content-[attr(data-l)]';

function Why() {
  return (
    <Section id="why">
      <h2 className={cn(H2, 'mb-5')}>A swarm of agents is not a team of people</h2>
      <p className={PARA}>It needs other things from source control.</p>

      <div className="mt-10 max-sm:mt-7">
        <div aria-hidden="true" className="grid grid-cols-2 gap-x-16 pb-3 max-sm:hidden">
          <span className={CAP}>The problem</span>
          <span className={CAP}>What SwarmOps does</span>
        </div>
        {PAIRS.map((pair) => (
          <div
            key={pair.problem}
            className="grid grid-cols-2 gap-x-16 border-t border-border pt-7 pb-8 max-sm:grid-cols-1 max-sm:gap-y-5 max-sm:pt-6 max-sm:pb-7"
          >
            <div data-l="The problem" className={PAIR_HALF}>
              <h3 className={cn(H3, 'mb-1.5')}>{pair.problem}</h3>
              <p>{pair.problemText}</p>
            </div>
            <div data-l="What SwarmOps does" className={PAIR_HALF}>
              <h3 className={cn(H3, 'mb-1.5')}>{pair.answer}</h3>
              <p>{pair.answerText}</p>
            </div>
          </div>
        ))}
      </div>
    </Section>
  );
}

// ---------------------------------------------------------------------------------------------
// 03 What it does today: README.md, "Core features", one guide per feature.

const FEATURES: { title: string; text: ReactNode; guide: string; url: string }[] = [
  {
    title: 'Land',
    text: (
      <>
        <C>land</C> merges a change three-way onto a ref&apos;s head, whatever the ref gained since, without a checkout,
        and moves the ref to the result in one transaction.
      </>
    ),
    guide: 'Commit and land',
    url: '/docs/guides/commit-and-land',
  },
  {
    title: 'Workspaces',
    text: (
      <>
        <C>so ws create</C> writes a changeset&apos;s tree into a directory of its own; the agent edits, builds and
        tests there, and <C>ws land --check</C> publishes the tree the check ran on.
      </>
    ),
    guide: 'Build and test a change',
    url: '/docs/operate/build-and-test',
  },
  {
    title: 'One JSON envelope per command',
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
    title: 'An MCP server for agents',
    text: (
      <>
        <C>swarmops mcp</C> serves the store to an agent on stdin and stdout: 38 tools, one per operation, each
        returning the envelope the CLI prints.
      </>
    ),
    guide: 'Claude Code over MCP',
    url: '/docs/guides/claude-code',
  },
  {
    title: 'Op log and undo',
    text: (
      <>
        <C>so op undo &lt;op&gt;</C> moves every ref that operation moved back to what it named before, all of them or
        none, by compare-and-swap.
      </>
    ),
    guide: 'History and undo',
    url: '/docs/guides/history-and-undo',
  },
  {
    title: 'Git import and export',
    text: (
      <>
        <C>git import</C> brings a git repository&apos;s history into a store, and <C>git export</C> writes it into a
        git branch, so that people, CI and deployment keep working in git.
      </>
    ),
    guide: 'Git round trip',
    url: '/docs/guides/git-round-trip',
  },
  {
    title: 'GitHub as a remote',
    text: (
      <>
        <C>push</C> sends a store ref to a branch by compare-and-swap, never by force, and <C>sync</C> lands the
        store&apos;s unpublished changes on top of the remote&apos;s head.
      </>
    ),
    guide: 'GitHub as a remote',
    url: '/docs/guides/github-remote',
  },
  {
    title: 'Crash safety and verify',
    text: (
      <>
        Durability is full fsync, with no fast mode. <C>verify --deep</C> re-hashes every object and checks the derived
        values against the objects.
      </>
    ),
    guide: 'Verify and repair',
    url: '/docs/operate/verify-and-repair',
  },
];

function Today() {
  return (
    <Section id="today">
      <h2 className={cn(H2, 'mb-5')}>Core features</h2>
      <p className={PARA}>Each one has a guide with commands you can run and their real output.</p>
      <div className="mt-10 grid grid-cols-2 gap-x-16 max-sm:mt-7 max-sm:grid-cols-1">
        {FEATURES.map((feature) => (
          <div key={feature.title} className="border-t border-border pt-6 pb-7 max-sm:py-5">
            <h3 className={cn(H3, 'mb-1.5')}>{feature.title}</h3>
            <p className="mb-2.5">{feature.text}</p>
            <a className={cn(LINK_44, 'gap-2 text-[15px]')} href={feature.url}>
              {feature.guide}
              <Arrow />
            </a>
          </div>
        ))}
      </div>
    </Section>
  );
}

// ---------------------------------------------------------------------------------------------
// 04 Where it stands: README.md's status sentence and docs/goals.md, each goal as last measured.

type Verdict = 'met' | 'part' | 'miss';

const VERDICT_ICON: Record<Verdict, ReactNode> = {
  met: <path d="m3 8.5 3 3 7-7" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />,
  part: (
    <>
      <circle cx="8" cy="8" r="5.5" fill="none" stroke="currentColor" strokeWidth="1.8" />
      <path d="M8 2.5a5.5 5.5 0 0 1 0 11z" fill="currentColor" />
    </>
  ),
  miss: (
    <>
      <circle cx="8" cy="8" r="5.5" fill="none" stroke="currentColor" strokeWidth="1.8" />
      <path d="M5.5 8h5" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" />
    </>
  ),
};

// A verdict is a word and an icon, never colour alone; a partial or missed one is quieter.
function VerdictMark({ verdict, children }: { verdict: Verdict; children: ReactNode }) {
  const met = verdict === 'met';
  return (
    <span className={cn('inline-flex items-center gap-2', met ? 'font-semibold text-foreground' : 'text-[var(--text)]')}>
      <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true" className={cn('flex-none', !met && 'text-muted-foreground')}>
        {VERDICT_ICON[verdict]}
      </svg>
      {children}
    </span>
  );
}

const GOALS: { id: string; name: string; target: ReactNode; measured: ReactNode; verdict: Verdict; word: string }[] = [
  {
    id: 'G1',
    name: 'Many writers, one store',
    target: (
      <>
        10 to 50 processes; no <C>E_BUSY</C> and no lost commit
      </>
    ),
    measured: (
      <>
        50 × 1,000 commits: <C>E_BUSY</C> 0, none lost (v0.6.0)
      </>
    ),
    verdict: 'met',
    word: 'Met',
  },
  {
    id: 'G2',
    name: 'Cheap commits, syncs excluded',
    target: <>≤ 5 ms + 0.3 ms per changed file</>,
    measured: <>p99 1.77 ms for 1 file, 13.50 ms for 100 (v0.6.0)</>,
    verdict: 'met',
    word: 'Met',
  },
  {
    id: 'G4',
    name: "Landing under a swarm's load, at 50 processes",
    target: <>≥ 20 lands/s, p99 &lt; 500 ms, retries &lt; 5 %, nothing lost</>,
    measured: <>25.0 lands/s, 0 % retries, nothing lost; p99 2,501–2,731 ms on a slower disk (v0.7.0), 579.5–594.6 ms in v0.6.0</>,
    verdict: 'part',
    word: 'Met but for p99',
  },
  {
    id: 'G17',
    name: 'One commit on 12,000 commits against 1,000',
    target: <>At most 1.5× as costly</>,
    measured: <>push 1.02×; fetch 1.34× (v0.7.0)</>,
    verdict: 'met',
    word: 'Met',
  },
];

// At 390 the table becomes one card per goal, each cell under its own label.
const CELL = 'border-b border-border px-3 py-3 text-left align-top max-sm:border-0 max-sm:px-0 max-sm:pt-1.5 max-sm:pb-0';
const CELL_LABEL =
  'max-sm:before:block max-sm:before:text-[13px]/5 max-sm:before:font-semibold max-sm:before:text-muted-foreground max-sm:before:content-[attr(data-l)]';

function Stands() {
  const { version, preRelease } = LATEST_RELEASE;
  return (
    <Section id="stands">
      <h2 className={cn(H2, 'mb-5')}>{preRelease ? `Pre-release, at v${version}` : `At v${version}`}</h2>
      <p className={PARA}>
        Usable by one agent, with a store that already takes commits from many processes at once; merging and landing,
        which a swarm needs, are built, and landing under a swarm&apos;s load is measured, within its targets but for
        the p99 at 50 processes; a store clones, fetches, pushes and syncs with GitHub itself; stock git clones,
        fetches and pushes a store that <C>swarmops serve</C> serves; and an agent builds and tests its change in a
        workspace on disk, then lands the tree it tested.
      </p>

      {/* 1. The goals that "usable" asks for, each with its target, measurement and verdict. */}
      <div className="mt-9 overflow-x-auto">
        <table className="w-full border-collapse text-[16px]/[26px] max-sm:block">
          <caption className="pb-3 text-left text-[14px]/6 text-muted-foreground max-sm:block">
            Goals as last measured, from{' '}
            <a className={LINK} href="/docs/goals">
              the goals page
            </a>
            : G1 and G2 in v0.6.0 (2026-10-28), G4 and G17 in v0.7.0 (2026-10-29)
          </caption>
          <thead className="max-sm:hidden">
            <tr>
              {['Goal', 'Target', 'Measured', 'Verdict'].map((head) => (
                <th key={head} scope="col" className="border-b-2 border-foreground px-3 py-2 text-left text-[14px] font-semibold text-foreground">
                  {head}
                </th>
              ))}
            </tr>
          </thead>
          <tbody className="max-sm:block">
            {GOALS.map((goal) => (
              <tr key={goal.id} className="max-sm:flex max-sm:flex-col max-sm:border-t max-sm:border-input max-sm:pt-4 max-sm:pb-[18px]">
                <th scope="row" className={cn(CELL, 'font-semibold text-foreground max-sm:order-2')}>
                  <span className="block font-mono text-[13px]/5 font-normal text-muted-foreground">{goal.id}</span>
                  {goal.name}
                </th>
                <td data-l="Target" className={cn(CELL, CELL_LABEL, 'w-[28%] max-sm:order-3 max-sm:w-auto')}>
                  {goal.target}
                </td>
                <td data-l="Measured" className={cn(CELL, CELL_LABEL, 'w-[30%] max-sm:order-3 max-sm:w-auto')}>
                  {goal.measured}
                </td>
                <td className={cn(CELL, 'whitespace-nowrap max-sm:order-1 max-sm:pt-0')}>
                  <VerdictMark verdict={goal.verdict}>{goal.word}</VerdictMark>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      {/* 2. The pilot's verdict, and what comes next. */}
      <blockquote className="mt-14 max-w-[720px] border-l-2 border-foreground py-1 pl-7 max-sm:pl-[18px]">
        <p className="font-serif text-[26px]/9 text-foreground max-sm:text-[22px]/8">
          Every criterion of the runbook passed; nothing was lost or rewritten.
        </p>
        <footer className="mt-2.5 text-[14px]/6 text-muted-foreground">
          The pilot on a real repository of 993 commits,{' '}
          <a className={cn(LINK, 'max-sm:inline-flex max-sm:min-h-11 max-sm:items-center')} href="/docs/changelog#050---2026-10-27">
            CHANGELOG 0.5.0
          </a>
        </footer>
      </blockquote>
      <p className={cn(PARA, 'mt-10')}>Next: v0.8.0 is operations.</p>
      <p className="mt-2 flex flex-wrap gap-x-6">
        <a className={LINK_44} href="/roadmap">
          Roadmap
        </a>
        <a className={LINK_44} href="/docs/goals">
          Goals
        </a>
        <a className={LINK_44} href="/docs/perf">
          Performance ledger
        </a>
      </p>
    </Section>
  );
}

// ---------------------------------------------------------------------------------------------
// 05 Fit: README.md, "SwarmOps and git", in three columns.

const FIT: { title: string; text: string }[] = [
  {
    title: 'Use SwarmOps',
    text: 'Where many agents on one host commit into one repository at once and need machine-readable answers, a single order of writes, no stale locks and a land that merges and retries by itself.',
  },
  {
    title: 'Keep git',
    text: 'For everything around it: review, CI, deployment, hosting, tags, hooks, signing, submodules and LFS. The store imports from git and exports to it, so git stays the system of record.',
  },
  {
    title: 'Expect to pay',
    text: 'In throughput and disk: writers take turns under one lock with full fsync, and objects are stored one file each, with no packs.',
  },
];

function Fit() {
  return (
    <Section id="fit">
      <h2 className={cn(H2, 'mb-5')}>SwarmOps and git</h2>
      <p className={PARA}>
        SwarmOps is not meant as a general-purpose git replacement: it imports from git and exports to it.
      </p>
      <div className="mt-10 grid grid-cols-3 gap-x-12 max-sm:mt-0 max-sm:grid-cols-1">
        {FIT.map((col) => (
          <div key={col.title} className="border-t-2 border-foreground pt-4 max-sm:mt-6">
            <h3 className={cn(H3, 'mb-1.5')}>{col.title}</h3>
            <p className="text-[16px]/[26px]">{col.text}</p>
          </div>
        ))}
      </div>
      <p className="mt-6">
        <a className={LINK} href="/product/vs-git">
          SwarmOps and git, dimension by dimension
        </a>
      </p>
    </Section>
  );
}

// ---------------------------------------------------------------------------------------------
// 06 Read on: docs/README.md's pages, grouped by what the reader wants to do.

const PATHS: { cap: string; who: string; links: { text: string; url: string; about: ReactNode }[] }[] = [
  {
    cap: 'Try it',
    who: 'For teams that run many coding agents on one host.',
    links: [
      {
        text: 'Install',
        url: '/docs/guides/install',
        about: (
          <>
            Install <C>swarmops</C> and <C>so</C> with the installer, from a release archive or from source; upgrade or
            remove them.
          </>
        ),
      },
      { text: 'Quick start', url: '/docs/guides/quick-start', about: <>Make a store, commit, land a change, meet a conflict and read the op log.</> },
    ],
  },
  {
    cap: 'Give it to agents',
    who: 'For people who build agent harnesses.',
    links: [
      { text: 'Claude Code', url: '/docs/guides/claude-code', about: <>Give Claude Code agents the store through MCP.</> },
      {
        text: 'MCP integration',
        url: '/docs/develop/mcp',
        about: <>Connect any MCP client: registration, the stdio exchange, identity, every tool and its command, limits and failures.</>,
      },
    ],
  },
  {
    cap: 'Run it',
    who: 'For operators who keep git as the system of record.',
    links: [
      { text: 'Operations', url: '/docs/operations', about: <>Find the task page for running SwarmOps for a swarm.</> },
      { text: 'Troubleshooting', url: '/docs/troubleshooting', about: <>Go from a symptom to its cause and the command that fixes it.</> },
      {
        text: 'Error reference',
        url: '/docs/reference/errors',
        about: (
          <>
            Look up an <C>E_*</C> code, its exit code and what to do.
          </>
        ),
      },
    ],
  },
];

function ReadOn() {
  return (
    <Section id="read-on">
      <h2 className={cn(H2, 'mb-5')}>The documents, grouped by what you want to do</h2>
      <div className="mt-9 grid grid-cols-3 gap-x-12 max-sm:mt-0 max-sm:grid-cols-1">
        {PATHS.map((path) => (
          <div key={path.cap} className="max-sm:mt-6">
            <p className={CAP}>{path.cap}</p>
            <p className="mt-1.5 min-h-12 text-[15px]/6 text-muted-foreground max-sm:min-h-0">{path.who}</p>
            <ul className="mt-3 list-none p-0">
              {path.links.map((link) => (
                <li key={link.url} className="border-t border-border py-3.5 max-sm:pt-1.5">
                  <a className={cn(LINK, 'text-[17px] font-semibold max-sm:flex max-sm:min-h-11 max-sm:items-center')} href={link.url}>
                    {link.text}
                  </a>
                  <span className="mt-0.5 block text-[15px]/6 text-muted-foreground">{link.about}</span>
                </li>
              ))}
            </ul>
          </div>
        ))}
      </div>
    </Section>
  );
}

export default function HomePage() {
  return (
    <main id="main" className="text-[17px]/7 text-[var(--text)]">
      <Hero />
      <JumpList />
      <SeeIt />
      <Why />
      <Today />
      <Stands />
      <Fit />
      <ReadOn />
    </main>
  );
}

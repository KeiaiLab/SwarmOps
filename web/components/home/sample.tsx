// Eight agents commit into one store at once: the transcript of docs/guides/parallel-agents.md,
// "SwarmOps: eight agents commit and land at once", recorded from a run. The command in a select
// figure, the output below it, folded after its first 8 lines; check-site's home-samples rule
// holds each output line to that transcript.
import { Fragment, type ReactNode } from 'react';
import { SelectFigure } from '@/components/markdown-page';
import { C, H2, LINK, PARA, Section } from './parts';

type Token = 'str' | 'flag' | 'var' | 'com';

// The docs' code colours (source.config.ts gives them to Shiki, global.css to these tokens);
// comments in muted.
const TOKEN_CLASS: Record<Token, string> = {
  str: 'text-code-string',
  flag: 'text-code-flag',
  var: 'text-code-variable',
  com: 'text-muted-foreground',
};

function T({ k, children }: { k: Token; children: ReactNode }) {
  return <span className={TOKEN_CLASS[k]}>{children}</span>;
}

const LANDED = [
  'landed b661f17c… on main (onto 3d63f551…, attempt 1, relocated)',
  'landed 662c74ed… on main (onto b661f17c…, attempt 1, relocated)',
  'landed e616ec6c… on main (onto dc448545…, attempt 1)',
  'landed 3d63f551… on main (onto 55eeb7ed…, attempt 1, relocated)',
  'landed 67506989… on main (onto e616ec6c…, attempt 1, relocated)',
  'landed 55eeb7ed… on main (onto 2db19b84…, attempt 1, relocated)',
  'landed 7a254784… on main (onto 662c74ed…, attempt 1, relocated)',
  'landed 2db19b84… on main (onto 67506989…, attempt 1, relocated)',
];

const ENTRIES = Array.from({ length: 8 }, (_, i) => `entries[${i}].name: ${i + 1}.txt`);

// Output wraps after , : { [ / instead of scrolling, as source.config.ts does for the docs.
const BREAK_AFTER = /([,:{[/])/;

/** Output lines with a break opportunity after each separator: program output, so pre > samp. */
export function OutputText({ lines }: { lines: string[] }) {
  return (
    <pre className="code-out-text">
      <samp>
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
    </pre>
  );
}

export function Sample() {
  return (
    <Section id="sample" n={2} label="See it">
      <h2 id="sample-title" className={H2}>
        Many agents commit into one store at once
      </h2>
      <p className={PARA + ' mt-4'}>
        Eight agent processes, all at once:
      </p>

      {/* 1. The command and what it printed, one figure; the listing folds after its first 8 lines. */}
      <div className="code-ex mt-8">
        <SelectFigure label="Shell" tabIndex={0}>
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
            {'\n'}printf <T k="str">{`'[{"path": "README.md", "op": "put", "content": {"inline": "# acme\\\\n"}}]'`}</T> &gt; first.json
            {'\n'}so <T k="flag">--as</T> agent/worker-0 <T k="flag">--human</T> commit <T k="flag">-m</T>{' '}
            <T k="str">&quot;first&quot;</T> <T k="flag">--changes</T> first.json <T k="flag">--advance</T> main &gt;/dev/null
            {'\n'}for i in 1 2 3 4 5 6 7 8; do work <T k="var">$i</T> &gt; land-<T k="var">$i</T>.txt &amp; done; wait
            {'\n'}cat land-*.txt
            {'\n'}so <T k="flag">--human</T> ls main:notes | grep name
          </code>
        </SelectFigure>
        <figure className="code-out not-prose">
          <figcaption>
            <span className="code-label">Output</span>
          </figcaption>
          <OutputText lines={LANDED} />
          <details className="code-fold">
            <summary>
              <span>Show {ENTRIES.length} more lines</span>
            </summary>
            <OutputText lines={ENTRIES} />
          </details>
        </figure>
      </div>

      {/* 2. What the output says. */}
      <p className={PARA + ' mt-6'}>
        All eight landed, on the first attempt. <C>relocated</C>: <C>main</C> moved under the land, which put the
        change on the new head itself.{' '}
        <a className={LINK} href="/docs/guides/parallel-agents">
          Parallel agents
        </a>{' '}
        compares this with git worktrees.
      </p>
    </Section>
  );
}

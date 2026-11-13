// README.md, "Many agents commit into one store at once", with its real output: the command in a
// select figure, the output below it, folded after its first 8 lines.
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

/** Output lines with a break opportunity after each separator. */
export function OutputText({ lines }: { lines: string[] }) {
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

export function Sample() {
  return (
    <Section id="sample" n={2} label="See it">
      <h2 id="sample-title" className={H2}>
        Many agents commit into one store at once
      </h2>
      <p className={PARA + ' mt-4'}>
        Each agent process writes the store directly. Here eight agents each commit a note and land it on <C>main</C>,
        all at once:
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

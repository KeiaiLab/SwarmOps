// Unit tests of web/lib/status-line.ts, the grammar of docs/status/<milestone>/<slice-id>.txt:
//
//   node --experimental-strip-types --test 'web/test/*.test.ts'
import assert from 'node:assert/strict';
import { test } from 'node:test';

import { checkSeed, parseStatusPath, parseStatusText } from '../lib/status-line.ts';

const COMMIT = '3a561c9179a1f2b3c4d5e6f708192a3b4c5d6e7f';

test('status_line_parses', () => {
  // One sentence and its newline; no seed commit.
  assert.deepEqual(parseStatusText('FORMAT 2 stores upgrade to FORMAT 3.\n'), {
    sentence: 'FORMAT 2 stores upgrade to FORMAT 3.',
    seed: null,
  });

  // A seed file names the commit that landed the slice.
  assert.deepEqual(parseStatusText(`Health renders as text.\ncommit ${COMMIT}\n`), {
    sentence: 'Health renders as text.',
    seed: COMMIT,
  });
});

test('status_name_grammar', () => {
  // A milestone directory and a slice id file name.
  assert.deepEqual(parseStatusPath('M20', 'M20-0A.txt'), { milestone: 'M20', slice: 'M20-0A' });
  assert.deepEqual(parseStatusPath('M21', 'M21-13-a.txt'), { milestone: 'M21', slice: 'M21-13-a' });

  // Anything else fails the build, naming the path.
  assert.throws(() => parseStatusPath('m20', 'M20-1.txt'), /docs\/status\/m20: not a milestone directory/);
  assert.throws(() => parseStatusPath('M20.txt', 'M20-1.txt'), /not a milestone directory/);
  assert.throws(() => parseStatusPath('M20', 'm20-1.txt'), /docs\/status\/M20\/m20-1\.txt: not a slice file name/);
  assert.throws(() => parseStatusPath('M20', 'M20-1.md'), /not a slice file name/);
  assert.throws(() => parseStatusPath('M20', 'M20 1.txt'), /not a slice file name/);
});

test('status_over_160_fails', () => {
  // 160 characters pass, 161 fail.
  assert.equal(parseStatusText(`${'a'.repeat(160)}\n`).sentence.length, 160);
  assert.throws(() => parseStatusText(`${'a'.repeat(161)}\n`), /161 characters, at most 160/);

  // An empty sentence fails too.
  assert.throws(() => parseStatusText('\n'), /one sentence/);
});

test('status_two_lines_without_commit_fails', () => {
  // A second line that is not `commit <40 hex>` fails, as does a third line.
  assert.throws(() => parseStatusText('First.\nSecond.\n'), /second line must be `commit <40 hex>`/);
  assert.throws(() => parseStatusText(`First.\ncommit ${COMMIT.slice(0, 8)}\n`), /second line must be/);
  assert.throws(() => parseStatusText(`First.\ncommit ${COMMIT}\nThird.\n`), /at most two lines/);
});

test('seed_commit_not_ancestor_fails', () => {
  // A seed commit HEAD reaches passes; any other fails the build.
  assert.doesNotThrow(() => checkSeed(COMMIT, () => true));
  assert.throws(() => checkSeed(COMMIT, () => false), new RegExp(`${COMMIT} is not an ancestor of HEAD`));
});

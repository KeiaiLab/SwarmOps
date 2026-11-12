// The grammar of a milestone status file, no imports, so node's test runner loads this file as
// it is (web/test/status.test.ts). One file per landing:
//
//   docs/status/M20/M20-4.txt      FORMAT 2 stores upgrade to FORMAT 3 in place; gc collects ...
//                                  commit 3a561c91...   (seed files only: the slice's commit)
//
// Its date and commit are git's (the commit that added the file), unless a seed line names one.

/** Where a status file sits: `docs/status/M20/M20-4.txt` -> M20, M20-4. */
export interface StatusPath {
  milestone: string;
  slice: string;
}

/** A status file's text: its sentence and, for a seed file, the commit it names. */
export interface StatusText {
  sentence: string;
  seed: string | null;
}

export const STATUS_ROOT = 'docs/status';

const MILESTONE_RE = /^M[0-9]+$/;
const FILE_RE = /^[A-Z][A-Za-z0-9.-]+\.txt$/;
const FILE_EXT = '.txt';
const SEED_RE = /^commit ([0-9a-f]{40})$/;
const SENTENCE_MAX = 160;

/** The milestone and slice id of `docs/status/<dir>/<file>`; any other shape fails the build. */
export function parseStatusPath(dir: string, file: string): StatusPath {
  if (!MILESTONE_RE.test(dir)) {
    throw new Error(`${STATUS_ROOT}/${dir}: not a milestone directory (M<n>)`);
  }
  if (!FILE_RE.test(file)) {
    throw new Error(`${STATUS_ROOT}/${dir}/${file}: not a slice file name (<slice-id>.txt)`);
  }
  return { milestone: dir, slice: file.slice(0, -FILE_EXT.length) };
}

/** One sentence of at most 160 characters, then an optional `commit <40 hex>` line; else throws. */
export function parseStatusText(text: string): StatusText {
  // 1. Lines, the file's final newline dropped.
  const lines = text.endsWith('\n') ? text.slice(0, -1).split('\n') : text.split('\n');
  if (lines.length > 2) {
    throw new Error(`at most two lines (sentence, commit), found ${lines.length}`);
  }

  // 2. The sentence.
  const sentence = lines[0];
  if (sentence.trim() === '') {
    throw new Error('the first line must be one sentence');
  }
  if (sentence.length > SENTENCE_MAX) {
    throw new Error(`sentence of ${sentence.length} characters, at most ${SENTENCE_MAX}`);
  }

  // 3. The seed commit, if any.
  if (lines.length === 1) {
    return { sentence, seed: null };
  }
  const seed = SEED_RE.exec(lines[1]);
  if (!seed) {
    throw new Error('the second line must be `commit <40 hex>`');
  }
  return { sentence, seed: seed[1] };
}

/** Fails the build unless `reachable` says HEAD reaches the seed commit. */
export function checkSeed(commit: string, reachable: (commit: string) => boolean): void {
  if (!reachable(commit)) {
    throw new Error(`seed commit ${commit} is not an ancestor of HEAD`);
  }
}

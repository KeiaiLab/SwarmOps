// When a docs file last changed, from git at build time:
//
//   docs/guides/install.md   git log -1 --format=%cs -- docs/guides/install.md   -> 2026-11-07
//   docs/new-page.md         not tracked yet                                      -> ''
//
// A shallow clone would date every page by its one commit, so it fails the build (lib/git.ts).
import { git, requireHistory } from './git';

/** Whether git tracks the file. */
function tracked(repoPath: string): boolean {
  try {
    git(['ls-files', '--error-unmatch', '--', repoPath]);
    return true;
  } catch {
    return false;
  }
}

const dates = new Map<string, string>();

/** The commit date (`2026-11-07`) of the file's last change, or '' for a file git does not track. */
export function lastChanged(repoPath: string): string {
  const cached = dates.get(repoPath);
  if (cached !== undefined) {
    return cached;
  }
  requireHistory();

  // 1. A new file has no history yet: its page shows no date.
  if (!tracked(repoPath)) {
    dates.set(repoPath, '');
    return '';
  }

  // 2. A tracked file always has a last commit; none means the history is broken.
  const date = git(['log', '-1', '--format=%cs', '--', repoPath]);
  if (date === '') {
    throw new Error(`docs-meta: ${repoPath} is tracked but git log gives it no date`);
  }
  dates.set(repoPath, date);
  return date;
}
